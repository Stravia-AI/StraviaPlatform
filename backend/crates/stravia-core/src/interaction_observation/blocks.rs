//! thinking / visible 块的正文拆分与回填。
//!
//! 正文位置由 runtime-contract 的 `item_content_units` 唯一决定：写入时把
//! 这些位置的字符串/JSON 取走，外壳（envelope）保留其余结构；读取时按同一
//! 指针顺序把正文填回外壳，再派生出事件视图需要的 `text` 与 `parts`。

use anyhow::{Context, ensure};
use serde_json::Value;
use stravia_runtime_contract::protocol::ir::{
    AiItem,
    content::item_content_units,
};

use super::content_refs::{text_content, unit_content};
use crate::content_store::{Body, Content};

/// 块的展示通道。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum BlockChannel {
    Thinking,
    Visible,
}

/// 拆分结果：正文单元按指针顺序排列，外壳里对应位置留空。
pub(super) struct SplitBlock {
    pub units: Vec<Content>,
    pub envelope: Option<Value>,
}

/// 拆分一个 IR item；无法解析的旧载荷退化为一个文本单元、无外壳（读取时按纯文本展示），
/// 退化会留下告警，且不记录载荷内容。
pub(super) fn split(item: Option<&Value>, fallback_text: &str) -> SplitBlock {
    let Some(item) = item else {
        return SplitBlock {
            units: vec![text_content(fallback_text)],
            envelope: None,
        };
    };
    let typed = match serde_json::from_value::<AiItem>(item.clone()) {
        Ok(typed) => typed,
        Err(error) => {
            tracing::warn!(%error, "observation block item is not a canonical AI item; storing its text only");
            return SplitBlock {
                units: vec![text_content(fallback_text)],
                envelope: None,
            };
        }
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
///
/// 外壳与正文必须一一对应：外壳无法解析、正文数量与指针数量不符、指针不在外壳里或正文格式不符
/// 都说明存储已损坏，直接返回错误，不拼出缺一段的视图。
pub(super) fn fill(
    envelope: Option<&Value>,
    bodies: &[&Body],
    channel: BlockChannel,
) -> anyhow::Result<FilledBlock> {
    let Some(envelope) = envelope else {
        return Ok(FilledBlock {
            item: None,
            parts: Vec::new(),
            text: join_text(bodies)?,
        });
    };
    let typed = serde_json::from_value::<AiItem>(envelope.clone())
        .context("stored block envelope is not a canonical AI item")?;
    let pointers: Vec<String> = item_content_units(&typed)
        .into_iter()
        .map(|unit| unit.pointer)
        .collect();
    ensure!(
        pointers.len() == bodies.len(),
        "block has {} content units but {} bodies",
        pointers.len(),
        bodies.len()
    );
    let mut filled = envelope.clone();
    for (pointer, body) in pointers.iter().zip(bodies) {
        set_pointer(&mut filled, pointer, body)?;
    }
    let text = match channel {
        BlockChannel::Thinking => join_text(bodies)?,
        BlockChannel::Visible => {
            let item = serde_json::from_value::<AiItem>(filled.clone())
                .context("restored block is not a canonical AI item")?;
            match item.output_text_ref().or_else(|| item.refusal_ref()) {
                Some(text) => text.to_owned(),
                None => join_text(bodies)?,
            }
        }
    };
    Ok(FilledBlock {
        parts: parts_of(&filled),
        item: Some(filled),
        text,
    })
}

fn join_text(bodies: &[&Body]) -> anyhow::Result<String> {
    Ok(bodies
        .iter()
        .map(|body| body.text())
        .collect::<anyhow::Result<Vec<_>>>()?
        .join("\n\n"))
}

fn parts_of(item: &Value) -> Vec<Value> {
    match item.get("content") {
        Some(Value::Array(parts)) => parts.clone(),
        Some(Value::String(text)) => vec![serde_json::json!({"type": "text", "text": text})],
        _ => Vec::new(),
    }
}

/// 外壳里正文位置清空为空串/null，保持外壳仍是合法的 AI item。
fn clear_pointer(shell: &mut Value, pointer: &str) {
    if let Some(target) = shell.pointer_mut(pointer) {
        match target {
            Value::String(text) => text.clear(),
            other => *other = Value::Null,
        }
    }
}

fn set_pointer(shell: &mut Value, pointer: &str, body: &Body) -> anyhow::Result<()> {
    let target = shell
        .pointer_mut(pointer)
        .with_context(|| format!("block envelope has no content slot {pointer}"))?;
    *target = body.value()?;
    Ok(())
}
