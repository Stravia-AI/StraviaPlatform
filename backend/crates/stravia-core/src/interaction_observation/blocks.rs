//! thinking / visible 块的正文拆分与回填。
//!
//! 正文位置由 runtime-contract 的 `item_content_units` 唯一决定：写入时把
//! 这些位置的字符串/JSON 取走，外壳（envelope）保留其余结构；读取时按同一
//! 指针顺序把正文填回外壳，再派生出事件视图需要的 `text` 与 `parts`。

use serde_json::Value;
use stravia_runtime_contract::protocol::ir::{
    AiItem,
    content::{ContentFormat, item_content_units},
};

use super::content_refs::{text_content, unit_content};
use crate::content_store::{Body, Content};

/// 块的展示通道。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum BlockChannel {
    Thinking,
    Visible,
}

impl BlockChannel {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Thinking => "thinking",
            Self::Visible => "visible",
        }
    }
}

/// 拆分结果：正文单元按指针顺序排列，外壳里对应位置留空。
pub(super) struct SplitBlock {
    pub units: Vec<Content>,
    pub envelope: Option<Value>,
}

/// 拆分一个 IR item；无法解析的旧载荷退化为一个文本单元、无外壳。
pub(super) fn split(item: Option<&Value>, fallback_text: &str) -> SplitBlock {
    let Some(item) = item else {
        return SplitBlock {
            units: vec![text_content(fallback_text)],
            envelope: None,
        };
    };
    let Ok(typed) = serde_json::from_value::<AiItem>(item.clone()) else {
        return SplitBlock {
            units: vec![text_content(fallback_text)],
            envelope: None,
        };
    };
    let mut units = Vec::new();
    let mut pointers = Vec::new();
    for unit in item_content_units(&typed) {
        pointers.push(unit.pointer.clone());
        units.push(unit_content(&unit));
    }
    let mut shell = item.clone();
    for pointer in &pointers {
        clear_pointer(&mut shell, pointer);
    }
    SplitBlock {
        units,
        envelope: Some(shell),
    }
}

/// 回填后的块视图。
pub(super) struct FilledBlock {
    pub item: Option<Value>,
    pub parts: Vec<Value>,
    pub text: String,
}

/// 把正文按指针顺序填回外壳，并派生展示字段。
pub(super) fn fill(
    envelope: Option<&Value>,
    bodies: &[&Body],
    channel: BlockChannel,
) -> FilledBlock {
    let Some(envelope) = envelope else {
        let text = join_text(bodies);
        return FilledBlock {
            item: None,
            parts: Vec::new(),
            text,
        };
    };
    let Ok(typed) = serde_json::from_value::<AiItem>(envelope.clone()) else {
        return FilledBlock {
            item: Some(envelope.clone()),
            parts: parts_of(envelope),
            text: join_text(bodies),
        };
    };
    let pointers: Vec<String> = item_content_units(&typed)
        .into_iter()
        .map(|unit| unit.pointer)
        .collect();
    let mut filled = envelope.clone();
    for (pointer, body) in pointers.iter().zip(bodies) {
        if let Err(error) = set_pointer(&mut filled, pointer, body) {
            tracing::warn!(%error, pointer, "observation block body could not be restored");
        }
    }
    let text = match channel {
        BlockChannel::Thinking => join_text(bodies),
        BlockChannel::Visible => serde_json::from_value::<AiItem>(filled.clone())
            .ok()
            .and_then(|item| {
                item.output_text_ref()
                    .or_else(|| item.refusal_ref())
                    .map(str::to_owned)
            })
            .unwrap_or_else(|| join_text(bodies)),
    };
    FilledBlock {
        parts: parts_of(&filled),
        item: Some(filled),
        text,
    }
}

fn join_text(bodies: &[&Body]) -> String {
    bodies
        .iter()
        .map(|body| body.text().unwrap_or_default())
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn parts_of(item: &Value) -> Vec<Value> {
    match item.get("content") {
        Some(Value::Array(parts)) => parts.clone(),
        Some(Value::String(text)) => vec![serde_json::json!({"type": "text", "text": text})],
        _ => Vec::new(),
    }
}

fn pointer_segments(pointer: &str) -> impl Iterator<Item = &str> {
    pointer.trim_start_matches('/').split('/')
}

fn navigate_mut<'a>(root: &'a mut Value, pointer: &str) -> Option<&'a mut Value> {
    let mut current = root;
    for segment in pointer_segments(pointer) {
        current = match current {
            Value::Object(map) => map.get_mut(segment)?,
            Value::Array(items) => items.get_mut(segment.parse::<usize>().ok()?)?,
            _ => return None,
        };
    }
    Some(current)
}

fn clear_pointer(shell: &mut Value, pointer: &str) {
    if let Some(target) = navigate_mut(shell, pointer) {
        match target {
            Value::String(text) => text.clear(),
            other => *other = Value::Null,
        }
    }
}

fn set_pointer(shell: &mut Value, pointer: &str, body: &Body) -> anyhow::Result<()> {
    let Some(target) = navigate_mut(shell, pointer) else {
        return Ok(());
    };
    match body.format {
        ContentFormat::Text => {
            *target = Value::String(body.text()?.to_owned());
        }
        ContentFormat::Json => {
            *target = body.json()?;
        }
    }
    Ok(())
}
