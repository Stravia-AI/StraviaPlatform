//! 正文 descriptor：`{"data": <正文位置为 null 的载荷>, "slots": [...]}`。
//!
//! 槽位按恢复顺序排列，父容器先于嵌套槽。三种形态：
//! `[pointer, ordinal]` 是 JSON 单元；`[pointer, ordinal, "t"]` 是文本单元；
//! `[pointer, ordinal, "t", prefix, suffix]` 是拼接文本槽，值为 `prefix + 正文 + suffix`。
//! `ordinal` 只指向调用方的引用表，descriptor 不携带内容 ID。

use anyhow::{Context, ensure};
use serde::de::{self, SeqAccess, Visitor};
use serde::ser::SerializeSeq;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;

/// 文本格式标记；descriptor JSON 中的字面量。
const TEXT_MARKER: &str = "t";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SlotKind {
    /// 紧凑 JSON 单元。
    Json,
    /// 原始 UTF-8 文本单元。
    Text,
    /// 前后缀字面量加文本单元。
    Concat { prefix: String, suffix: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Slot {
    pub(crate) pointer: String,
    pub(crate) ordinal: usize,
    pub(crate) kind: SlotKind,
}

impl Slot {
    pub(crate) fn json(pointer: String, ordinal: usize) -> Self {
        Self {
            pointer,
            ordinal,
            kind: SlotKind::Json,
        }
    }

    pub(crate) fn text(pointer: String, ordinal: usize) -> Self {
        Self {
            pointer,
            ordinal,
            kind: SlotKind::Text,
        }
    }

    pub(crate) fn concat(pointer: String, ordinal: usize, prefix: String, suffix: String) -> Self {
        Self {
            pointer,
            ordinal,
            kind: SlotKind::Concat { prefix, suffix },
        }
    }
}

impl Serialize for Slot {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match &self.kind {
            SlotKind::Json => {
                let mut sequence = serializer.serialize_seq(Some(2))?;
                sequence.serialize_element(&self.pointer)?;
                sequence.serialize_element(&self.ordinal)?;
                sequence.end()
            }
            SlotKind::Text => {
                let mut sequence = serializer.serialize_seq(Some(3))?;
                sequence.serialize_element(&self.pointer)?;
                sequence.serialize_element(&self.ordinal)?;
                sequence.serialize_element(TEXT_MARKER)?;
                sequence.end()
            }
            SlotKind::Concat { prefix, suffix } => {
                let mut sequence = serializer.serialize_seq(Some(5))?;
                sequence.serialize_element(&self.pointer)?;
                sequence.serialize_element(&self.ordinal)?;
                sequence.serialize_element(TEXT_MARKER)?;
                sequence.serialize_element(prefix)?;
                sequence.serialize_element(suffix)?;
                sequence.end()
            }
        }
    }
}

impl<'de> Deserialize<'de> for Slot {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_seq(SlotVisitor)
    }
}

struct SlotVisitor;

impl<'de> Visitor<'de> for SlotVisitor {
    type Value = Slot;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("a content slot array")
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut sequence: A) -> Result<Slot, A::Error> {
        let pointer: String = sequence
            .next_element()?
            .ok_or_else(|| de::Error::invalid_length(0, &self))?;
        let ordinal: usize = sequence
            .next_element()?
            .ok_or_else(|| de::Error::invalid_length(1, &self))?;
        let marker: Option<String> = sequence.next_element()?;
        match marker.as_deref() {
            None => {
                ensure_trailing_empty(&mut sequence)?;
                Ok(Slot::json(pointer, ordinal))
            }
            Some(TEXT_MARKER) => {
                let prefix: Option<String> = sequence.next_element()?;
                match prefix {
                    None => {
                        ensure_trailing_empty(&mut sequence)?;
                        Ok(Slot::text(pointer, ordinal))
                    }
                    Some(prefix) => {
                        let suffix: String = sequence
                            .next_element()?
                            .ok_or_else(|| de::Error::invalid_length(4, &self))?;
                        ensure_trailing_empty(&mut sequence)?;
                        Ok(Slot::concat(pointer, ordinal, prefix, suffix))
                    }
                }
            }
            Some(other) => Err(de::Error::unknown_variant(other, &[TEXT_MARKER])),
        }
    }
}

fn ensure_trailing_empty<'de, A: SeqAccess<'de>>(sequence: &mut A) -> Result<(), A::Error> {
    if sequence.next_element::<serde::de::IgnoredAny>()?.is_some() {
        return Err(de::Error::custom("trailing content slot element"));
    }
    Ok(())
}

#[derive(serde::Serialize)]
struct DescriptorRef<'a> {
    data: &'a Value,
    slots: &'a [Slot],
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Descriptor {
    pub(crate) data: Value,
    pub(crate) slots: Vec<Slot>,
}

/// 编码 descriptor；`data` 中的正文位置必须已经是 null。
pub(crate) fn encode_descriptor(data: &Value, slots: &[Slot]) -> anyhow::Result<Vec<u8>> {
    let descriptor = serde_json::to_vec(&DescriptorRef { data, slots })?;
    crate::storage_codec::encode(std::borrow::Cow::Owned(descriptor))
}

pub(crate) fn decode_descriptor(encoded: &[u8]) -> anyhow::Result<Descriptor> {
    let raw = crate::storage_codec::decode(encoded)?;
    Ok(serde_json::from_slice(&raw)?)
}

/// 校验槽位覆盖：ordinal 必须在 `mapping` 范围内，且每个 ordinal 至少被一个槽使用。
pub(crate) fn validate_slots(descriptor: &Descriptor, mapping_len: usize) -> anyhow::Result<()> {
    let mut seen = vec![false; mapping_len];
    for slot in &descriptor.slots {
        ensure!(
            slot.ordinal < mapping_len,
            "content slot ordinal out of range"
        );
        seen[slot.ordinal] = true;
    }
    ensure!(
        seen.iter().all(|referenced| *referenced),
        "unreferenced content ordinal"
    );
    Ok(())
}

/// 按槽位顺序把正文还原到 `data`。父容器必须先于嵌套槽恢复。
pub(crate) fn restore_slots(
    data: &mut Value,
    slots: &[Slot],
    mut resolve: impl FnMut(usize) -> anyhow::Result<Value>,
) -> anyhow::Result<()> {
    for slot in slots {
        let body = resolve(slot.ordinal)?;
        let target = data
            .pointer_mut(&slot.pointer)
            .with_context(|| format!("invalid content slot {}", slot.pointer))?;
        ensure!(target.is_null(), "content slot overwrites data");
        *target = match &slot.kind {
            SlotKind::Json => body,
            SlotKind::Text => {
                ensure!(matches!(body, Value::String(_)), "content slot is not text");
                body
            }
            SlotKind::Concat { prefix, suffix } => {
                let Value::String(text) = body else {
                    anyhow::bail!("content slot is not text");
                };
                let capacity = prefix
                    .len()
                    .saturating_add(text.len())
                    .saturating_add(suffix.len());
                let mut joined = String::with_capacity(capacity);
                joined.push_str(prefix);
                joined.push_str(&text);
                joined.push_str(suffix);
                Value::String(joined)
            }
        };
    }
    Ok(())
}
