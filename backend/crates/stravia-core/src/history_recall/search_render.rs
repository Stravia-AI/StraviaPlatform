//! 将索引事件区间映射为可读取的历史路径与安全摘要。

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::ops::Range;

use stravia_runtime_contract::hook::PlatformToolError;
use stravia_web_access_contract::read_path::HISTORY_ROOT;

use crate::mcp::read::text::PAGE_BYTES;

use super::{
    Recall, RecallInteraction, Transcript,
    page::ChunkMark,
    render,
    search_store::SearchSegment,
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

/// 每个 Interaction 最多列出的不同命中位置数。
const HITS_PER_INTERACTION: usize = 3;

#[derive(Clone)]
pub(super) struct RenderedHit {
    pub(super) location: String,
    pub(super) snippet: String,
    pub(super) after_first_page: bool,
}

/// 命中在可读文件中的位置；位置相同的命中摘要也相同，因此按位置去重。
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum HitSpot {
    /// 转录中的绝对行范围。
    Transcript { first: u64, last: u64 },
    /// 第 `number` 个工具文件中的行范围。
    Tool { number: u32, first: u64, last: u64 },
}

impl Recall<'_> {
    /// 一次遍历转录，按段的优先顺序返回前 [`HITS_PER_INTERACTION`] 个不同位置的命中及其所属段。
    ///
    /// 长单行跨多个索引段时，这些段换算到同一行范围，只列一次，名额留给下一个不同位置。
    /// 工具文件只为入选的命中读取。
    pub(super) async fn render_hits<'s>(
        &self,
        interaction: &RecallInteraction,
        segments: &'s [SearchSegment],
        query: &ParsedQuery,
    ) -> Result<Vec<(&'s SearchSegment, RenderedHit)>, PlatformToolError> {
        let mut spots = vec![None; segments.len()];
        let mut remaining = segments.len();
        if remaining == 0 {
            return Ok(Vec::new());
        }
        let base = format!("{HISTORY_ROOT}{}/{}", interaction.root_id, interaction.id);
        // 转录摘要只能在遍历到对应事件时生成，按位置缓存；工具文件在选定后再读取。
        let mut transcript_hits: HashMap<HitSpot, RenderedHit> = HashMap::new();
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
            let renderer = EventRenderer::new(&event, &chunk, &indexed.text, query);
            for (index, segment) in segments.iter().enumerate() {
                if segment.event_sequence != event.sequence {
                    continue;
                }
                let start =
                    usize::try_from(segment.byte_start).map_err(|_| changed_during_search())?;
                let end = usize::try_from(segment.byte_end).map_err(|_| changed_during_search())?;
                let spot = renderer.locate(start, end)?;
                if let HitSpot::Transcript { first, last } = spot
                    && !transcript_hits.contains_key(&spot)
                {
                    transcript_hits.insert(spot, renderer.transcript_hit(&base, first, last)?);
                }
                spots[index] = Some(spot);
                remaining -= 1;
            }
            if remaining == 0 {
                break;
            }
        }
        let mut selected: Vec<(&SearchSegment, HitSpot)> = Vec::with_capacity(HITS_PER_INTERACTION);
        for (segment, spot) in segments.iter().zip(spots) {
            let spot = spot.ok_or_else(changed_during_search)?;
            if selected.len() < HITS_PER_INTERACTION
                && !selected.iter().any(|(_, listed)| *listed == spot)
            {
                selected.push((segment, spot));
            }
        }
        let mut tool_files: HashMap<u32, Vec<String>> = HashMap::new();
        let mut hits = Vec::with_capacity(selected.len());
        for (segment, spot) in selected {
            let hit = match spot {
                HitSpot::Transcript { .. } => transcript_hits
                    .remove(&spot)
                    .ok_or_else(changed_during_search)?,
                HitSpot::Tool {
                    number,
                    first,
                    last,
                } => {
                    let lines = match tool_files.entry(number) {
                        Entry::Occupied(entry) => entry.into_mut(),
                        Entry::Vacant(entry) => {
                            let tool = self
                                .observation
                                .recall_tool(&interaction.id, number)
                                .await
                                .map_err(storage)?
                                .ok_or_else(changed_during_search)?;
                            entry.insert(render::tool_file(
                                interaction,
                                number,
                                &tool,
                                &self.secrets,
                            ))
                        }
                    };
                    let selected = lines
                        .get((first - 1) as usize..last as usize)
                        .ok_or_else(changed_during_search)?
                        .join("\n");
                    RenderedHit {
                        location: format!("{base}/tools/{number}:{first}-{last}"),
                        snippet: snippet(&selected, query),
                        after_first_page: hit_after_first_page(&selected, query),
                    }
                }
            };
            hits.push((segment, hit));
        }
        Ok(hits)
    }
}

/// 把同一事件内的索引段换算为命中位置。
struct EventRenderer<'a> {
    chunk: &'a super::Chunk,
    text: &'a str,
    query: &'a ParsedQuery,
    hits: Vec<Range<usize>>,
    /// `text` 中各 LF 的字节位置；一个事件可能有大量命中段，行号用二分换算。
    newlines: Vec<usize>,
    tool: bool,
    argument_start: usize,
    /// 该事件在转录中的标题行下标；只在需要时因缺失而报错。
    heading: Option<usize>,
}

impl<'a> EventRenderer<'a> {
    fn new(
        event: &super::ObservationEvent,
        chunk: &'a super::Chunk,
        text: &'a str,
        query: &'a ParsedQuery,
    ) -> Self {
        let tool = matches!(
            event.kind.as_str(),
            "client_tool_handoff" | "platform_tool_started"
        );
        let heading = chunk.lines.iter().position(|line| {
            if tool {
                line.starts_with("### tool call ")
            } else if event.kind == "user_input_recorded" {
                line == "## user"
            } else {
                line == "## assistant"
            }
        });
        Self {
            chunk,
            text,
            query,
            hits: matches(text, query),
            newlines: text.match_indices('\n').map(|(index, _)| index).collect(),
            tool,
            argument_start: text.find('\n').unwrap_or(text.len()).saturating_add(1),
            heading,
        }
    }

    /// 字节位置之前的 LF 数，即该位置在正文中的 0-based 行偏移；正文按 LF 切行，
    /// 因此行范围在转录追加事件后仍稳定。
    fn line_of(&self, byte: usize) -> u64 {
        self.newlines.partition_point(|&newline| newline < byte) as u64
    }

    /// 段内首个命中的位置。工具名与参数命中同时存在时优先参数：参数一律映射到完整
    /// 工具文件，工具名才映射到转录的 `### tool call` 行。
    fn locate(&self, start: usize, end: usize) -> Result<HitSpot, PlatformToolError> {
        let text = self.text;
        if start >= end
            || end > text.len()
            || !text.is_char_boundary(start)
            || !text.is_char_boundary(end)
        {
            return Err(changed_during_search());
        }
        let within = |range: &&Range<usize>| range.start >= start && range.end <= end;
        let argument_start = self.argument_start;
        if self.tool
            && let Some(argument) = self
                .hits
                .iter()
                .filter(within)
                .find(|range| range.end > argument_start)
        {
            let base = self.line_of(argument_start);
            let first_line = render::TOOL_FILE_HEADER_LINES + 1;
            return Ok(HitSpot::Tool {
                number: self.chunk.mark.tools.saturating_add(1),
                first: first_line + self.line_of(argument.start.max(argument_start)) - base,
                last: first_line + self.line_of(argument.end - 1) - base,
            });
        }
        let heading = self.chunk.mark.line + self.heading.ok_or_else(changed_during_search)? as u64;
        if self.tool {
            return Ok(HitSpot::Transcript {
                first: heading,
                last: heading,
            });
        }
        let hit = self.hits.iter().find(within).cloned().unwrap_or(start..end);
        let body = heading + 1;
        Ok(HitSpot::Transcript {
            first: body + self.line_of(hit.start),
            last: body + self.line_of(hit.end.saturating_sub(1)),
        })
    }

    /// 转录行范围的命中；摘要只取渲染后（已替换凭据）的行。
    fn transcript_hit(
        &self,
        base: &str,
        first: u64,
        last: u64,
    ) -> Result<RenderedHit, PlatformToolError> {
        let first_index = (first - self.chunk.mark.line) as usize;
        let last_index = (last - self.chunk.mark.line + 1) as usize;
        let selected = self
            .chunk
            .lines
            .get(first_index..last_index)
            .ok_or_else(changed_during_search)?
            .join("\n");
        Ok(RenderedHit {
            location: format!("{base}:{first}-{last}"),
            snippet: snippet(&selected, self.query),
            after_first_page: hit_after_first_page(&selected, self.query),
        })
    }
}
