//! Marker 段载荷的共享正文外挂与恢复。
//!
//! `segment_payload` 从未压缩 TEXT 改为 storage_codec 编码的 descriptor；`call`、
//! `source`、`signature`、`encrypted_content` 等留在外壳内，正文单元（思考文本、
//! 工具结果 content）经 `history_marker_contents` 引用。正文位置同样只由 IR 的
//! 正文单元定义给出，不按 JSON 键名推断。

use std::collections::HashMap;

use anyhow::Context;
use serde_json::Value;

use stravia_runtime_contract::protocol::ir::content::{
    ContentFormat, ContentUnitBody, block_content_units,
};

use super::HiddenHistorySegment;
use crate::content_store::{self, Bodies, Content, ContentDigest, Slot};

pub(crate) struct SegmentPayload {
    /// storage_codec 编码的 descriptor；写入 `segment_payload`。
    pub(crate) envelope: Vec<u8>,
    /// 去重后的正文；下标就是引用表的 ordinal。
    pub(crate) contents: Vec<Content>,
}

/// 把段载荷拆成外壳与正文单元。
pub(crate) fn encode_segment(segment: &HiddenHistorySegment) -> anyhow::Result<SegmentPayload> {
    let mut data = serde_json::to_value(segment)?;
    let mut units = Vec::new();
    match segment {
        HiddenHistorySegment::Platform { result, .. } => {
            block_content_units(result, "/result", &mut units);
        }
        HiddenHistorySegment::Thinking { block, .. } => {
            block_content_units(block, "/block", &mut units);
        }
    }
    let mut contents = Vec::new();
    let mut index: HashMap<ContentDigest, usize> = HashMap::new();
    let mut slots = Vec::new();
    for unit in units {
        let Some(target) = data.pointer_mut(&unit.pointer) else {
            continue;
        };
        if target.is_null() {
            continue;
        }
        let (format, raw, text) = match unit.body {
            ContentUnitBody::Text(text) => (ContentFormat::Text, text.as_bytes().to_vec(), true),
            ContentUnitBody::Json(_) => (ContentFormat::Json, serde_json::to_vec(&*target)?, false),
        };
        let digest = content_store::content_digest(format, &raw);
        let ordinal = match index.get(&digest) {
            Some(ordinal) => *ordinal,
            None => {
                let ordinal = contents.len();
                index.insert(digest, ordinal);
                contents.push(Content::with_digest(digest, format, raw));
                ordinal
            }
        };
        slots.push(if text {
            Slot::text(unit.pointer, ordinal)
        } else {
            Slot::json(unit.pointer, ordinal)
        });
        *target = Value::Null;
    }
    let envelope = content_store::encode_descriptor(&data, &slots)?;
    Ok(SegmentPayload { envelope, contents })
}

/// 按引用恢复段载荷。`mapping[ordinal]` 是内容 ID。
pub(super) fn restore_segment(
    envelope: &[u8],
    mapping: &[i64],
    bodies: &Bodies,
) -> anyhow::Result<HiddenHistorySegment> {
    let mut descriptor = content_store::decode_descriptor(envelope)?;
    content_store::validate_slots(&descriptor, mapping.len())?;
    content_store::restore_slots(&mut descriptor.data, &descriptor.slots, |ordinal| {
        let id = *mapping
            .get(ordinal)
            .context("history marker slot ordinal out of range")?;
        let body = bodies.get(id).context("missing history marker content")?;
        body.value()
    })?;
    Ok(serde_json::from_value(descriptor.data)?)
}
