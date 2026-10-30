use serde_json::Value;

// 延续 precommit 的保守口径，为稀疏 map 节点、键和值槽留出空间；不是分配器或 RSS 测量。
const JSON_OBJECT_ENTRY_BYTES: usize = 4 * std::mem::size_of::<(String, Value)>();

pub(crate) fn json_retained_bytes(value: &Value) -> usize {
    std::mem::size_of::<Value>().saturating_add(json_heap_bytes(value))
}

/// 只计堆存储；调用方已计入内联 Value、事件或其他容器的固定大小。
pub(crate) fn json_heap_bytes(value: &Value) -> usize {
    match value {
        Value::String(text) => text.capacity(),
        // arbitrary_precision 保留数字词法，但不公开 String 容量；借用计数并保守预留扩容空间。
        Value::Number(number) => number.as_str().len().saturating_mul(2),
        Value::Array(values) => values.iter().fold(
            values
                .capacity()
                .saturating_mul(std::mem::size_of::<Value>()),
            |bytes, value| bytes.saturating_add(json_heap_bytes(value)),
        ),
        Value::Object(values) => json_object_heap_bytes(values.iter()),
        Value::Null | Value::Bool(_) => 0,
    }
}

pub(crate) fn json_object_heap_bytes<'a>(
    entries: impl Iterator<Item = (&'a String, &'a Value)>,
) -> usize {
    entries.fold(0usize, |bytes, (key, value)| {
        bytes
            .saturating_add(JSON_OBJECT_ENTRY_BYTES)
            .saturating_add(key.capacity())
            .saturating_add(json_heap_bytes(value))
    })
}
