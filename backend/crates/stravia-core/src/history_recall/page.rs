//! 文件页的行预算与游标：沿用文本读取的 32 KiB / 200 行 / 16 段预算。
//!
//! 行号是稳定坐标；字节数会随凭据替换变化，所以游标与 `returned_ranges` 只记录行号，
//! 仅在单行超过整页预算时额外记录行内字节偏移。

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use stravia_runtime_contract::hook::PlatformToolError;
use stravia_web_access_contract::read_path::LineSelection;

use crate::interaction_observation::RecallInteraction;
use crate::mcp::read::text::{PAGE_BYTES, PAGE_LINES};

const CURSOR_LIMIT: usize = 8192;
const RANGE_LIMIT: usize = 16;
/// 区间上界取此值表示到文件末尾。
const TO_END: u64 = u64::MAX;

pub(super) fn invalid_cursor() -> PlatformToolError {
    PlatformToolError::new("Invalid history cursor")
}

pub(super) fn stale_cursor() -> PlatformToolError {
    PlatformToolError::new("History cursor no longer matches this entry; read the path again")
}

pub(super) fn outside() -> PlatformToolError {
    PlatformToolError::new("Line selection is outside the history file")
}

pub(super) fn encode<T: Serialize>(cursor: &T) -> Result<String, PlatformToolError> {
    let bytes =
        serde_json::to_vec(cursor).map_err(|error| PlatformToolError::new(error.to_string()))?;
    let encoded = URL_SAFE_NO_PAD.encode(bytes);
    if encoded.len() > CURSOR_LIMIT {
        return Err(PlatformToolError::new(
            "Generated history cursor exceeds its size limit",
        ));
    }
    Ok(encoded)
}

pub(super) fn decode<T: DeserializeOwned>(encoded: &str) -> Result<T, PlatformToolError> {
    if encoded.len() > CURSOR_LIMIT {
        return Err(invalid_cursor());
    }
    let bytes = URL_SAFE_NO_PAD
        .decode(encoded)
        .map_err(|_| invalid_cursor())?;
    serde_json::from_slice(&bytes).map_err(|_| invalid_cursor())
}

/// 选择是否需要先知道总行数（`:-K`）。
pub(super) fn needs_total(selections: Option<&[LineSelection]>) -> bool {
    selections.is_some_and(|selections| {
        selections
            .iter()
            .any(|selection| matches!(selection, LineSelection::Last(_)))
    })
}

/// 把行选择换成升序、合并后的闭区间；未给选择时读全文。`total` 只在含 `:-K` 时需要。
pub(super) fn resolve(
    selections: Option<&[LineSelection]>,
    total: Option<u64>,
) -> Result<Vec<[u64; 2]>, PlatformToolError> {
    let Some(selections) = selections else {
        return Ok(vec![[1, TO_END]]);
    };
    if selections.is_empty() || selections.len() > RANGE_LIMIT {
        return Err(outside());
    }
    let mut ranges = Vec::with_capacity(selections.len());
    for selection in selections {
        let range = match *selection {
            LineSelection::From(start) => [start, TO_END],
            LineSelection::Inclusive { start, end } => [start, end],
            LineSelection::Count { start, count } => {
                let end = count
                    .checked_sub(1)
                    .and_then(|count| start.checked_add(count))
                    .ok_or_else(|| PlatformToolError::new("Line selection overflows"))?;
                [start, end]
            }
            LineSelection::Last(0) => {
                return Err(PlatformToolError::new("Line count must be positive"));
            }
            LineSelection::Last(count) => {
                let total = total.ok_or_else(outside)?;
                if total == 0 {
                    return Err(outside());
                }
                [total.saturating_sub(count).saturating_add(1), total]
            }
        };
        if range[0] == 0 || range[0] > range[1] {
            return Err(outside());
        }
        ranges.push(range);
    }
    ranges.sort_unstable();
    let mut merged: Vec<[u64; 2]> = Vec::with_capacity(ranges.len());
    for [start, end] in ranges {
        if let Some(previous) = merged.last_mut()
            && start <= previous[1].saturating_add(1)
        {
            previous[1] = previous[1].max(end);
            continue;
        }
        merged.push([start, end]);
    }
    Ok(merged)
}

/// 文件续读位置：剩余区间、下一行与行内字节偏移。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Position {
    pub(super) ranges: Vec<[u64; 2]>,
    pub(super) range_index: usize,
    pub(super) line: u64,
    pub(super) offset: usize,
}

impl Position {
    pub(super) fn start(ranges: Vec<[u64; 2]>) -> Self {
        let line = ranges.first().map_or(1, |range| range[0]);
        Self {
            ranges,
            range_index: 0,
            line,
            offset: 0,
        }
    }

    /// 游标是不可信输入：区间必须升序不相交，且续读行落在当前区间内。
    pub(super) fn validate(&self) -> Result<(), PlatformToolError> {
        if self.ranges.is_empty()
            || self.ranges.len() > RANGE_LIMIT
            || self.range_index >= self.ranges.len()
        {
            return Err(invalid_cursor());
        }
        let mut previous_end = 0;
        for &[start, end] in &self.ranges {
            if start == 0 || start > end || start <= previous_end {
                return Err(invalid_cursor());
            }
            previous_end = end;
        }
        let [start, end] = self.ranges[self.range_index];
        if self.line < start || self.line > end {
            return Err(invalid_cursor());
        }
        Ok(())
    }

    /// 最后一个区间的起点；初次读取需要确认所有起点都在文件内。
    pub(super) fn last_start(&self) -> u64 {
        self.ranges.last().map_or(1, |range| range[0])
    }
}

/// 转录块的起点：块所属事件（`None` 为头部）、首行行号，以及渲染该块前的工具编号数与上一个 Run。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ChunkMark {
    pub(super) event: Option<i64>,
    pub(super) line: u64,
    pub(super) tools: u32,
    pub(super) run: Option<String>,
}

impl ChunkMark {
    pub(super) fn header() -> Self {
        Self {
            event: None,
            line: 1,
            tools: 0,
            run: None,
        }
    }

    /// 事件归属与工具编号在重建时另行校验；这里只校验与续读行、Interaction 的一致性。
    pub(super) fn validate(
        &self,
        interaction: &RecallInteraction,
        line: u64,
    ) -> Result<(), PlatformToolError> {
        if self.line == 0 || self.line > line || (self.event.is_none() && *self != Self::header()) {
            return Err(invalid_cursor());
        }
        if let Some(run) = &self.run
            && !interaction.runs.contains_key(run)
        {
            return Err(invalid_cursor());
        }
        Ok(())
    }
}

/// 文件游标；转录带 `chunk`，工具文件不带。
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct FileCursor {
    pub(super) v: u8,
    pub(super) path: String,
    pub(super) position: Position,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) chunk: Option<ChunkMark>,
}

pub(super) enum Offer {
    /// 本行已写入或不在选择内，继续给下一行。
    Continue,
    /// 页已满或选择已读完，不再需要后续行。
    Finished,
}

/// 逐行填充一页。`offer` 必须按行号升序调用。
pub(super) struct Page {
    position: Position,
    content: String,
    lines: usize,
    returned: Vec<[u64; 2]>,
    next: Option<Position>,
    finished: bool,
}

impl Page {
    pub(super) fn new(position: Position) -> Self {
        Self {
            position,
            content: String::new(),
            lines: 0,
            returned: Vec::new(),
            next: None,
            finished: false,
        }
    }

    /// 续读的起始行与行内偏移。
    pub(super) fn resume_at(&self) -> (u64, usize) {
        (self.position.line, self.position.offset)
    }

    pub(super) fn finished(&self) -> bool {
        self.finished
    }

    /// `from` 是本行的起始字节，只在续读的第一行非零；越界或不在字符边界时游标已失效。
    pub(super) fn offer(
        &mut self,
        number: u64,
        line: &str,
        from: usize,
    ) -> Result<Offer, PlatformToolError> {
        if self.finished {
            return Ok(Offer::Finished);
        }
        loop {
            let Some(&[start, end]) = self.position.ranges.get(self.position.range_index) else {
                self.finished = true;
                return Ok(Offer::Finished);
            };
            if number > end {
                self.position.range_index += 1;
                continue;
            }
            if number < start {
                return Ok(Offer::Continue);
            }
            break;
        }
        let rest = line.get(from..).ok_or_else(stale_cursor)?;
        if self.lines == PAGE_LINES
            || (!self.content.is_empty() && self.content.len() + rest.len() > PAGE_BYTES)
        {
            return Ok(self.stop(number, from));
        }
        if rest.len() > PAGE_BYTES {
            // 单行超过整页预算：在字符边界切开，下一页从行内偏移继续。
            let mut cut = PAGE_BYTES;
            while !rest.is_char_boundary(cut) {
                cut -= 1;
            }
            self.content.push_str(&rest[..cut]);
            self.record(number);
            self.lines += 1;
            return Ok(self.stop(number, from + cut));
        }
        self.content.push_str(rest);
        self.content.push('\n');
        self.record(number);
        self.lines += 1;
        Ok(Offer::Continue)
    }

    fn stop(&mut self, line: u64, offset: usize) -> Offer {
        self.next = Some(Position {
            ranges: self.position.ranges.clone(),
            range_index: self.position.range_index,
            line,
            offset,
        });
        self.finished = true;
        Offer::Finished
    }

    fn record(&mut self, number: u64) {
        if let Some(last) = self.returned.last_mut()
            && (last[1] == number || last[1].saturating_add(1) == number)
        {
            last[1] = number;
            return;
        }
        self.returned.push([number, number]);
    }

    /// 页结束后的内容、返回区间与续读位置（没有更多内容时为 `None`）。
    pub(super) fn finish(self) -> (String, Value, Option<Position>) {
        let returned = self
            .returned
            .iter()
            .map(|[start, end]| json!({"start_line": start, "end_line": end}))
            .collect();
        (self.content, Value::Array(returned), self.next)
    }
}

/// 目录、文件共用的页输出字段，与文本读取的页一致。
pub(super) fn output(
    address: &str,
    content: String,
    returned_ranges: Option<Value>,
    next_cursor: Option<String>,
) -> Value {
    let mut value = json!({
        "content": content,
        "read_path": address,
        "representation": "markdown",
        "has_more": next_cursor.is_some(),
        "limitations": [],
    });
    if let Some(ranges) = returned_ranges {
        value["returned_ranges"] = ranges;
    }
    if let Some(cursor) = next_cursor {
        value["next_path"] = json!(format!("{address}:cursor={cursor}"));
    }
    value
}
