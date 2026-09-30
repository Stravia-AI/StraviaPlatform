//! 强制按 Claude Code / oh-my-pi 的策略放置缓存断点，覆盖客户端自带的 `cache_control`。
//!
//! 客户端的断点无法信任：经 OpenAI Chat、Responses、Gemini 等入口进来的请求根本没有
//! 断点，上游不会建立缓存；即使 Anthropic 入口的客户端自带断点，位置也可能随对话追加
//! 落到中间消息上、破坏前缀，总数还可能超过上游的 4 个上限。因此先清掉全部客户端断点，
//! 再复刻 oh-my-pi `applyHeadCaching` 与 `applyPromptCaching`（`anthropic.ts`）重新放置：
//!
//! - 头部：最后一个非 deferred 工具、最后一个 system 块各一个断点，让体量大且稳定
//!   的「工具 + system」前缀在每一轮都命中；
//! - 消息：预算为 4 减去头部断点数，依次取最后一条消息、每 15 个对话轮次的稳定
//!   检查点、倒数第二条消息，使断点随对话滚动前移。
//!
//! 断点 TTL 统一为 1 小时：Claude Code 对订阅用户的主对话默认如此，且全部同 TTL 也
//! 满足上游「长 TTL 必须在前」的排序约束。

use serde_json::{Map, Value, json};

const MAX_CACHE_BREAKPOINTS: usize = 4;
const LONG_CACHE_TTL: &str = "1h";

/// 与 oh-my-pi `ANTHROPIC_DECIMATION_INTERVAL` 一致：每 15 个对话轮次钉一个历史锚点。
const DECIMATION_INTERVAL: usize = 15;

/// 上游拒绝在这些块上携带 `cache_control`（生成的推理内容、回退边界、工具增删控制）。
const UNCACHEABLE_BLOCK_TYPES: [&str; 5] = [
    "thinking",
    "redacted_thinking",
    "fallback",
    "tool_addition",
    "tool_removal",
];

fn breakpoint() -> Value {
    json!({ "type": "ephemeral", "ttl": LONG_CACHE_TTL })
}

/// 清除请求里全部已有断点，再按固定策略重新放置。
pub(crate) fn enforce(object: &mut Map<String, Value>) {
    strip_breakpoints(object);
    anchor_tools(object);
    anchor_system(object);
    let used = head_breakpoints(object);
    anchor_messages(object, MAX_CACHE_BREAKPOINTS.saturating_sub(used));
}

/// 只清理承载 `cache_control` 的位置：`system` / `tools` 条目、消息内容块，以及
/// `tool_result.content` 里的子块。不做递归清洗：工具 `input_schema` 或 `tool_use.input`
/// 里可能有名为 `cache_control` 的普通字段，那是客户端数据，不能动。
fn strip_breakpoints(object: &mut Map<String, Value>) {
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

fn anchor_tools(object: &mut Map<String, Value>) {
    let Some(Value::Array(tools)) = object.get_mut("tools") else {
        return;
    };
    // deferred 工具在被引用前不属于已校验前缀，断点要落在真正处于稳定前缀里的工具上。
    if let Some(tool) = tools
        .iter_mut()
        .rev()
        .filter_map(Value::as_object_mut)
        .find(|tool| tool.get("defer_loading").and_then(Value::as_bool) != Some(true))
    {
        tool.insert("cache_control".into(), breakpoint());
    }
}

/// 断点落在最后一个 system 块：规范顺序是 tools → system → messages，它一次覆盖
/// 整个 tools + system 前缀。
fn anchor_system(object: &mut Map<String, Value>) {
    let Some(Value::Array(system)) = object.get_mut("system") else {
        return;
    };
    if let Some(block) = system.last_mut().and_then(Value::as_object_mut) {
        block.insert("cache_control".into(), breakpoint());
    }
}

fn head_breakpoints(object: &Map<String, Value>) -> usize {
    ["system", "tools"]
        .into_iter()
        .filter_map(|key| object.get(key)?.as_array())
        .flatten()
        .filter(|entry| {
            entry
                .get("cache_control")
                .is_some_and(|value| !value.is_null())
        })
        .count()
}

/// 对话轮次由新的用户输入推进；只含 `tool_result` 的 user 消息属于同一轮的工具续接。
fn is_conversational_user(message: &Value) -> bool {
    if message.get("role").and_then(Value::as_str) != Some("user") {
        return false;
    }
    match message.get("content") {
        Some(Value::Array(blocks)) => blocks
            .iter()
            .any(|block| block.get("type").and_then(Value::as_str) != Some("tool_result")),
        _ => true,
    }
}

fn candidate_indices(messages: &[Value]) -> Vec<usize> {
    let last = messages.len() - 1;
    let checkpoints: Vec<usize> = messages
        .iter()
        .enumerate()
        .filter(|(_, message)| is_conversational_user(message))
        .map(|(index, _)| index)
        .enumerate()
        .filter(|(ordinal, _)| (ordinal + 1) % DECIMATION_INTERVAL == 0)
        .map(|(_, index)| index)
        .collect();
    let mut candidates = vec![last];
    for index in checkpoints.into_iter().rev() {
        if !candidates.contains(&index) {
            candidates.push(index);
        }
    }
    if last > 0 && !candidates.contains(&(last - 1)) {
        candidates.push(last - 1);
    }
    candidates
}

fn anchor_messages(object: &mut Map<String, Value>, budget: usize) {
    let Some(Value::Array(messages)) = object.get_mut("messages") else {
        return;
    };
    if budget == 0 || messages.is_empty() {
        return;
    }
    let mut applied = 0;
    for index in candidate_indices(messages) {
        if applied >= budget {
            break;
        }
        // 只统计真正成功的标注，避免仅含 thinking 的助手消息白占名额。
        if mark_message(&mut messages[index]) {
            applied += 1;
        }
    }
}

fn mark_message(message: &mut Value) -> bool {
    let Some(message) = message.as_object_mut() else {
        return false;
    };
    match message.get_mut("content") {
        Some(Value::String(text)) if !text.is_empty() => {
            let text = std::mem::take(text);
            message.insert(
                "content".into(),
                json!([{ "type": "text", "text": text, "cache_control": breakpoint() }]),
            );
            true
        }
        Some(Value::Array(blocks)) => mark_last_cacheable_block(blocks),
        _ => false,
    }
}

fn mark_last_cacheable_block(blocks: &mut [Value]) -> bool {
    for block in blocks.iter_mut().rev() {
        let Some(block) = block.as_object_mut() else {
            continue;
        };
        let kind = block
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if UNCACHEABLE_BLOCK_TYPES.contains(&kind) {
            continue;
        }
        // 上游拒绝空文本块上的 `cache_control`，与 thinking 一样向前找可标注块。
        if kind == "text" && block.get("text").and_then(Value::as_str) == Some("") {
            continue;
        }
        block.insert("cache_control".into(), breakpoint());
        return true;
    }
    false
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

    fn user(text: &str) -> Value {
        json!({"role": "user", "content": [self::text(text)]})
    }

    fn tool_round(id: &str) -> [Value; 2] {
        [
            json!({"role": "assistant", "content": [
                {"type": "thinking", "thinking": "t", "signature": "s"},
                {"type": "tool_use", "id": id, "name": "read", "input": {}}
            ]}),
            json!({"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": id, "content": "ok"}
            ]}),
        ]
    }

    fn request(messages: Vec<Value>) -> Map<String, Value> {
        json!({
            "tools": [
                {"name": "a", "input_schema": {}},
                {"name": "b", "input_schema": {}},
                {"name": "deferred", "input_schema": {}, "defer_loading": true}
            ],
            "system": [text("billing"), text("identity"), text("client")],
            "messages": messages,
        })
        .as_object()
        .unwrap()
        .clone()
    }

    fn total_breakpoints(object: &Map<String, Value>) -> usize {
        let messages = object["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|message| message["content"].as_array())
            .flatten()
            .filter(|block| marked(block))
            .count();
        head_breakpoints(object) + messages
    }

    #[test]
    fn head_anchors_last_stable_tool_and_last_system_block() {
        let mut object = request(vec![user("hi")]);
        enforce(&mut object);
        let tools = object["tools"].as_array().unwrap();
        assert!(!marked(&tools[0]));
        assert!(marked(&tools[1]), "last non-deferred tool");
        assert!(!marked(&tools[2]), "deferred tool is outside the prefix");
        let system = object["system"].as_array().unwrap();
        assert!(!marked(&system[0]) && !marked(&system[1]));
        assert_eq!(
            system[2]["cache_control"],
            json!({"type": "ephemeral", "ttl": "1h"})
        );
    }

    #[test]
    fn client_breakpoints_are_replaced_by_the_fixed_policy() {
        let client = json!({"type": "ephemeral"});
        let mut object = request(vec![
            json!({"role": "user", "content": [
                {"type": "text", "text": "old", "cache_control": client},
                {"type": "text", "text": "old2", "cache_control": client},
            ]}),
            json!({"role": "assistant", "content": [text("mid")]}),
            json!({"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "t", "content": [
                    {"type": "text", "text": "nested", "cache_control": client}
                ]}
            ]}),
        ]);
        object["system"][0]["cache_control"] = client.clone();
        object["tools"][0]["cache_control"] = client.clone();
        enforce(&mut object);

        assert!(!marked(&object["system"][0]));
        assert!(!marked(&object["tools"][0]));
        let messages = object["messages"].as_array().unwrap();
        assert!(!marked(&messages[0]["content"][0]));
        assert!(!marked(&messages[0]["content"][1]));
        assert!(!marked(&messages[2]["content"][0]["content"][0]));
        assert!(marked(&messages[2]["content"][0]), "tail is re-anchored");
        assert!(total_breakpoints(&object) <= MAX_CACHE_BREAKPOINTS);
    }

    #[test]
    fn schema_fields_named_cache_control_are_client_data() {
        let mut object = request(vec![json!({"role": "assistant", "content": [
            {"type": "tool_use", "id": "t", "name": "n", "input": {"cache_control": "keep"}}
        ]})]);
        object["tools"][0]["input_schema"] =
            json!({"properties": {"cache_control": {"type": "string"}}});
        enforce(&mut object);
        assert_eq!(
            object["messages"][0]["content"][0]["input"]["cache_control"],
            "keep"
        );
        assert!(object["tools"][0]["input_schema"]["properties"]["cache_control"].is_object());
    }

    #[test]
    fn message_budget_is_four_minus_head_and_tail_rolls_forward() {
        let mut messages = vec![user("task")];
        messages.extend(tool_round("t1"));
        messages.extend(tool_round("t2"));
        let mut object = request(messages);
        enforce(&mut object);
        assert_eq!(total_breakpoints(&object), 4);
        let messages = object["messages"].as_array().unwrap();
        assert!(marked(&messages[4]["content"][0]), "last message");
        assert!(
            marked(&messages[3]["content"][1]),
            "second trailing lands on tool_use, after the uncacheable thinking block"
        );
        assert!(!marked(&messages[0]["content"][0]));
    }

    #[test]
    fn thinking_blocks_never_carry_a_breakpoint() {
        let mut object = request(vec![
            user("a"),
            json!({"role": "assistant", "content": [
                {"type": "thinking", "thinking": "t", "signature": "s"}
            ]}),
            user("b"),
        ]);
        enforce(&mut object);
        let messages = object["messages"].as_array().unwrap();
        assert!(marked(&messages[2]["content"][0]));
        assert!(
            !marked(&messages[1]["content"][0]),
            "upstream rejects cache_control on thinking blocks"
        );
    }

    #[test]
    fn string_content_is_upgraded_to_a_text_block() {
        let mut object = request(vec![json!({"role": "user", "content": "hello"})]);
        enforce(&mut object);
        assert_eq!(
            object["messages"][0]["content"],
            json!([{
                "type": "text",
                "text": "hello",
                "cache_control": {"type": "ephemeral", "ttl": "1h"}
            }])
        );
    }

    #[test]
    fn empty_text_tail_falls_back_to_previous_block() {
        let mut object = request(vec![json!({"role": "user", "content": [
            text("real"), text("")
        ]})]);
        enforce(&mut object);
        let blocks = object["messages"][0]["content"].as_array().unwrap();
        assert!(marked(&blocks[0]));
        assert!(!marked(&blocks[1]));
    }

    #[test]
    fn long_conversation_pins_every_fifteenth_user_turn() {
        let mut messages = Vec::new();
        for turn in 0..16 {
            messages.push(user(&format!("q{turn}")));
            messages.push(json!({"role": "assistant", "content": [text("a")]}));
        }
        messages.push(user("latest"));
        let mut object = request(messages);
        enforce(&mut object);
        assert_eq!(total_breakpoints(&object), 4);
        let messages = object["messages"].as_array().unwrap();
        assert!(marked(&messages[28]["content"][0]), "15th user turn");
        assert!(marked(&messages[32]["content"][0]), "last message");
    }

    #[test]
    fn tool_result_only_user_messages_do_not_advance_the_turn_ordinal() {
        let mut messages = vec![user("task")];
        for round in 0..20 {
            messages.extend(tool_round(&format!("t{round}")));
        }
        let mut object = request(messages);
        enforce(&mut object);
        let messages = object["messages"].as_array().unwrap();
        let marked_messages: Vec<usize> = (0..messages.len())
            .filter(|index| {
                messages[*index]["content"]
                    .as_array()
                    .is_some_and(|blocks| blocks.iter().any(marked))
            })
            .collect();
        assert_eq!(
            marked_messages,
            vec![messages.len() - 2, messages.len() - 1]
        );
    }
}
