//! 按官方 Claude Code 2.1.317 实测请求放置缓存断点。
//!
//! 使用自定义 system 的交互式完整历史请求：
//!
//! - `tools` 从不携带断点；
//! - system 为 `[计费头(无断点), CLI 身份块(1h), 客户端 system(1h)]`；
//! - messages 只在最新一条消息的最后一个可缓存块上放一个 1h 断点，历史消息
//!   不再追加滚动锚点。
//!
//! 客户端自带的断点不可信（经 OpenAI/Gemini 等入口进来的请求根本没有，Anthropic
//! 入口的位置也会随对话追加漂移），所以 [`strip`] 在 prepare 阶段清掉全部槽位，
//! 断点只在 encode 阶段按上述 profile 重新放置，且从不修改调用方持有的历史。

use serde_json::{Map, Value, json};

const LONG_CACHE_TTL: &str = "1h";

/// 上游拒绝在这些块上携带 `cache_control`（生成的推理内容、回退边界、工具增删控制）。
const UNCACHEABLE_BLOCK_TYPES: [&str; 6] = [
    "thinking",
    "redacted_thinking",
    "fallback",
    "tool_addition",
    "tool_removal",
    // 压缩块必须逐字节回放，附加 `cache_control` 会使上游拒绝其签名。
    "compaction",
];

fn breakpoint() -> Value {
    json!({ "type": "ephemeral", "ttl": LONG_CACHE_TTL })
}

/// 只清理承载 `cache_control` 的位置：`system` / `tools` 条目、消息内容块，以及
/// `tool_result.content` 里的子块。不做递归清洗：工具 `input_schema` 或 `tool_use.input`
/// 里可能有名为 `cache_control` 的普通字段，那是客户端数据，不能动。
pub(crate) fn strip(object: &mut Map<String, Value>) {
    for key in ["system", "tools"] {
        if let Some(Value::Array(entries)) = object.get_mut(key) {
            entries
                .iter_mut()
                .filter_map(Value::as_object_mut)
                .for_each(|entry| drop(entry.remove("cache_control")));
        }
    }
    let Some(Value::Array(messages)) = object.get_mut("messages") else {
        return;
    };
    for block in messages
        .iter_mut()
        .filter_map(|message| message.get_mut("content"))
        .filter_map(Value::as_array_mut)
        .flatten()
        .filter_map(Value::as_object_mut)
    {
        block.remove("cache_control");
        if let Some(Value::Array(nested)) = block.get_mut("content") {
            nested
                .iter_mut()
                .filter_map(Value::as_object_mut)
                .for_each(|entry| drop(entry.remove("cache_control")));
        }
    }
}

/// 在计费头之后的 system 块上放断点：第一块（CLI 身份）与最后一块（客户端 system）。
/// 只有身份块时两者是同一块，只放一个。
pub(crate) fn anchor_system(blocks: &mut [Value]) {
    let last = blocks.len().saturating_sub(1);
    for index in [0, last].into_iter().take(if last == 0 { 1 } else { 2 }) {
        if let Some(block) = blocks.get_mut(index).and_then(Value::as_object_mut) {
            block.insert("cache_control".into(), breakpoint());
        }
    }
}

/// 在 `messages` 里最新的、含可缓存块的消息上放断点，返回该消息的索引与带断点的
/// 副本；不改动入参，也不复制其余历史。
pub(crate) fn anchor_tail(messages: &[Value]) -> Option<(usize, Value)> {
    messages
        .iter()
        .enumerate()
        .rev()
        .find_map(|(index, message)| {
            let block_index = match message.get("content")? {
                Value::String(text) if !text.is_empty() => None,
                Value::Array(blocks) => Some(blocks.iter().rposition(cacheable_block)?),
                _ => return None,
            };
            let mut marked = message.clone();
            if let Some(block_index) = block_index {
                marked["content"][block_index]
                    .as_object_mut()?
                    .insert("cache_control".into(), breakpoint());
            } else {
                let text = marked["content"].take();
                marked["content"] =
                    json!([{ "type": "text", "text": text, "cache_control": breakpoint() }]);
            }
            Some((index, marked))
        })
}

fn cacheable_block(block: &Value) -> bool {
    let Some(block) = block.as_object() else {
        return false;
    };
    let kind = block
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default();
    !UNCACHEABLE_BLOCK_TYPES.contains(&kind)
        && !(kind == "text" && block.get("text").and_then(Value::as_str) == Some(""))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn marked(value: &Value) -> bool {
        value.get("cache_control").is_some()
    }

    fn text(text: &str) -> Value {
        json!({"type": "text", "text": text})
    }

    #[test]
    fn strip_removes_only_cache_slots_and_keeps_client_data() {
        let client = json!({"type": "ephemeral", "ttl": "5m"});
        let mut object = json!({
            "tools": [{
                "name": "a",
                "cache_control": client,
                "input_schema": {"properties": {"cache_control": {"type": "string"}}}
            }],
            "system": [{"type": "text", "text": "s", "cache_control": client}],
            "messages": [
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "t", "name": "n", "input": {"cache_control": "keep"}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "t", "cache_control": client, "content": [
                        {"type": "text", "text": "nested", "cache_control": client}
                    ]}
                ]}
            ],
        })
        .as_object()
        .unwrap()
        .clone();
        strip(&mut object);
        assert!(!marked(&object["tools"][0]) && !marked(&object["system"][0]));
        assert!(object["tools"][0]["input_schema"]["properties"]["cache_control"].is_object());
        assert_eq!(
            object["messages"][0]["content"][0]["input"]["cache_control"],
            "keep"
        );
        let result = &object["messages"][1]["content"][0];
        assert!(!marked(result) && !marked(&result["content"][0]));
    }

    #[test]
    fn system_anchors_sit_on_identity_and_last_block_only() {
        let mut blocks = vec![text("identity"), text("client a"), text("client b")];
        anchor_system(&mut blocks);
        assert!(marked(&blocks[0]) && !marked(&blocks[1]) && marked(&blocks[2]));
        assert_eq!(
            blocks[2]["cache_control"],
            json!({"type": "ephemeral", "ttl": "1h"})
        );

        let mut only_identity = vec![text("identity")];
        anchor_system(&mut only_identity);
        assert!(marked(&only_identity[0]));
        anchor_system(&mut []);
    }

    #[test]
    fn tail_anchor_marks_one_block_without_touching_history() {
        let messages = vec![
            json!({"role": "user", "content": [text("first")]}),
            json!({"role": "assistant", "content": [text("answer")]}),
            json!({"role": "user", "content": [text("a"), text("b")]}),
        ];
        let before = messages.clone();
        let (index, tail) = anchor_tail(&messages).unwrap();
        assert_eq!(index, 2);
        assert!(!marked(&tail["content"][0]) && marked(&tail["content"][1]));
        assert_eq!(messages, before, "caller history is borrowed, never marked");
    }

    #[test]
    fn tail_anchor_does_not_roll_into_history() {
        // 单个断点：长对话不再 pin 历史锚点或倒数第二条消息。
        let mut messages = Vec::new();
        for turn in 0..40 {
            messages.push(json!({"role": "user", "content": [text(&format!("q{turn}"))]}));
            messages.push(json!({"role": "assistant", "content": [text("a")]}));
        }
        messages.push(json!({"role": "user", "content": [text("latest")]}));
        let (index, tail) = anchor_tail(&messages).unwrap();
        assert_eq!(index, messages.len() - 1);
        assert!(marked(&tail["content"][0]));
    }

    #[test]
    fn thinking_tail_falls_back_to_previous_cacheable_message() {
        let messages = vec![
            json!({"role": "user", "content": [text("a")]}),
            json!({"role": "assistant", "content": [
                {"type": "thinking", "thinking": "t", "signature": "s"}
            ]}),
        ];
        let (index, tail) = anchor_tail(&messages).unwrap();
        assert_eq!(
            index, 0,
            "upstream rejects cache_control on thinking blocks"
        );
        assert!(marked(&tail["content"][0]));
        assert!(anchor_tail(&messages[1..]).is_none());
    }

    #[test]
    fn string_content_is_upgraded_to_a_text_block() {
        let (_, tail) = anchor_tail(&[json!({"role": "user", "content": "hello"})]).unwrap();
        assert_eq!(
            tail["content"],
            json!([{
                "type": "text",
                "text": "hello",
                "cache_control": {"type": "ephemeral", "ttl": "1h"}
            }])
        );
    }

    #[test]
    fn empty_text_and_uncacheable_blocks_are_skipped_inside_a_message() {
        let (_, tail) = anchor_tail(&[json!({"role": "user", "content": [
            text("real"), text("")
        ]})])
        .unwrap();
        assert!(marked(&tail["content"][0]) && !marked(&tail["content"][1]));

        let (_, tail) = anchor_tail(&[json!({"role": "assistant", "content": [
            {"type": "tool_use", "id": "t", "name": "n", "input": {}},
            {"type": "compaction", "content": "c", "signature": "s"}
        ]})])
        .unwrap();
        assert!(marked(&tail["content"][0]) && !marked(&tail["content"][1]));
    }
}
