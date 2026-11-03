//! 正文单元：由 IR 类型唯一确定的共享正文位置。
//!
//! Turn Chain、History Marker 与 Interaction Observation 只调用这里的定义，
//! 不再按 JSON 键名推断正文。正文单元不设大小门槛，空字符串同样寻址。
//! 指针相对调用方给出的容器：item 的 `content` 从 `/content` 开始，
//! Marker 的 `result` 块从 `/result` 开始。

use serde_json::Value;

use super::{AiItem, ContentBlock, MessageContent};

/// 正文的存储格式；数值是持久化编码，不得复用。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum ContentFormat {
    /// 原始 UTF-8 文本，不做 JSON 转义。
    Text = 0,
    /// 紧凑 serde JSON。
    Json = 1,
}

impl ContentFormat {
    pub fn as_byte(self) -> u8 {
        self as u8
    }

    pub fn as_database_value(self) -> i64 {
        i64::from(self.as_byte())
    }

    pub fn from_database_value(value: i64) -> Option<Self> {
        match value {
            0 => Some(Self::Text),
            1 => Some(Self::Json),
            _ => None,
        }
    }
}

/// 正文内容本身；JSON 单元借用原值，避免序列化前复制。
#[derive(Debug, Clone, Copy)]
pub enum ContentUnitBody<'a> {
    Text(&'a str),
    Json(&'a Value),
}

impl ContentUnitBody<'_> {
    pub fn format(&self) -> ContentFormat {
        match self {
            Self::Text(_) => ContentFormat::Text,
            Self::Json(_) => ContentFormat::Json,
        }
    }
}

/// 一个正文单元：容器内的 JSON 指针、格式与内容。
#[derive(Debug, Clone)]
pub struct ContentUnit<'a> {
    pub pointer: String,
    pub body: ContentUnitBody<'a>,
}

impl ContentUnit<'_> {
    pub fn format(&self) -> ContentFormat {
        self.body.format()
    }
}

/// 追加一个容器内的文本正文单元。
fn text_unit<'a>(units: &mut Vec<ContentUnit<'a>>, pointer: String, text: &'a str) {
    units.push(ContentUnit {
        pointer,
        body: ContentUnitBody::Text(text),
    });
}

/// 追加一个容器内的 JSON 正文单元。
fn json_unit<'a>(units: &mut Vec<ContentUnit<'a>>, pointer: String, value: &'a Value) {
    units.push(ContentUnit {
        pointer,
        body: ContentUnitBody::Json(value),
    });
}

/// `AiItem` 的正文单元，指针相对 item 序列化后的对象。
pub fn item_content_units(item: &AiItem) -> Vec<ContentUnit<'_>> {
    let mut units = Vec::new();
    message_content_units(&item.content, "/content", &mut units);
    units
}

/// `MessageContent` 的正文单元；`pointer` 是内容字段本身的指针。
pub fn message_content_units<'a>(
    content: &'a MessageContent,
    pointer: &str,
    units: &mut Vec<ContentUnit<'a>>,
) {
    match content {
        // 纯文本消息序列化为字符串，正文就是该字段本身。
        MessageContent::Text(text) => text_unit(units, pointer.to_owned(), text.as_str()),
        MessageContent::Blocks(blocks) => {
            for (index, block) in blocks.iter().enumerate() {
                block_content_units(block, &format!("{pointer}/{index}"), units);
            }
        }
    }
}

/// 单个 `ContentBlock` 的正文单元；`pointer` 是块本身的指针。
pub fn block_content_units<'a>(
    block: &'a ContentBlock,
    pointer: &str,
    units: &mut Vec<ContentUnit<'a>>,
) {
    match block {
        ContentBlock::Text { text, .. } => {
            text_unit(units, format!("{pointer}/text"), text.as_str());
        }
        ContentBlock::Thinking { thinking, .. } => {
            text_unit(units, format!("{pointer}/thinking"), thinking);
        }
        ContentBlock::Reasoning {
            summary, content, ..
        } => {
            for (index, text) in summary.iter().enumerate() {
                text_unit(units, format!("{pointer}/summary/{index}"), text);
            }
            for (index, text) in content.iter().enumerate() {
                text_unit(units, format!("{pointer}/content/{index}"), text);
            }
        }
        // ToolResult / ServerToolResult 的 content：字符串是文本单元，
        // 其他 JSON（包括 `null` 与块数组）整段是 JSON 单元。
        ContentBlock::ToolResult { content, .. }
        | ContentBlock::ServerToolResult { content, .. } => match content {
            Value::String(text) => text_unit(units, format!("{pointer}/content"), text),
            other => json_unit(units, format!("{pointer}/content"), other),
        },
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::ir::{Role, ToolCallId, ToolResultContentKind};
    use serde_json::json;

    fn item(wire: Value) -> AiItem {
        serde_json::from_value(wire).expect("typed item")
    }

    fn pointers<'a>(units: &'a [ContentUnit<'a>]) -> Vec<(&'a str, ContentFormat)> {
        units
            .iter()
            .map(|unit| (unit.pointer.as_str(), unit.format()))
            .collect()
    }

    #[test]
    fn text_message_item_points_at_the_content_field() {
        let item = item(json!({"role": "user", "content": "hello"}));
        let units = item_content_units(&item);
        assert_eq!(pointers(&units), vec![("/content", ContentFormat::Text)]);
        assert!(matches!(units[0].body, ContentUnitBody::Text("hello")));
    }

    #[test]
    fn text_and_thinking_blocks_expose_their_own_fields() {
        let item = item(json!({
            "role": "assistant",
            "content": [
                {"type": "text", "text": "visible"},
                {"type": "thinking", "thinking": "private", "signature": "sig"},
            ]
        }));
        let units = item_content_units(&item);
        assert_eq!(
            pointers(&units),
            vec![
                ("/content/0/text", ContentFormat::Text),
                ("/content/1/thinking", ContentFormat::Text),
            ]
        );
        assert!(matches!(units[0].body, ContentUnitBody::Text("visible")));
        assert!(matches!(units[1].body, ContentUnitBody::Text("private")));
    }

    #[test]
    fn reasoning_summary_and_content_are_separate_units_in_order() {
        let item = item(json!({
            "role": "assistant",
            "content": [{
                "type": "reasoning",
                "summary": ["first", ""],
                "content": ["second"],
                "encrypted_content": "ciphertext"
            }]
        }));
        let units = item_content_units(&item);
        assert_eq!(
            pointers(&units),
            vec![
                ("/content/0/summary/0", ContentFormat::Text),
                ("/content/0/summary/1", ContentFormat::Text),
                ("/content/0/content/0", ContentFormat::Text),
            ]
        );
        assert!(matches!(units[1].body, ContentUnitBody::Text("")));
    }

    #[test]
    fn tool_result_string_is_text_and_other_json_is_json() {
        let item = item(json!({
            "role": "tool",
            "content": [
                {"type": "tool_result", "tool_use_id": "a", "content": "plain", "is_error": false},
                {"type": "tool_result", "tool_use_id": "b", "content": {"ok": [1, 2]}},
                {"type": "server_tool_result", "tool_use_id": "c", "content": [{"type": "text", "text": "nested"}]},
                {"type": "tool_result", "tool_use_id": "d", "content": null, "content_kind": ToolResultContentKind::Json},
            ]
        }));
        let units = item_content_units(&item);
        assert_eq!(
            pointers(&units),
            vec![
                ("/content/0/content", ContentFormat::Text),
                ("/content/1/content", ContentFormat::Json),
                ("/content/2/content", ContentFormat::Json),
                ("/content/3/content", ContentFormat::Json),
            ]
        );
        assert!(matches!(units[0].body, ContentUnitBody::Text("plain")));
        assert!(matches!(
            units[1].body,
            ContentUnitBody::Json(Value::Object(_))
        ));
        assert!(matches!(units[3].body, ContentUnitBody::Json(Value::Null)));
    }

    #[test]
    fn block_units_are_relative_to_the_supplied_pointer() {
        let block = ContentBlock::ToolResult {
            tool_use_id: ToolCallId::from("call"),
            content: Value::String("body".into()),
            content_kind: None,
            is_error: None,
            cache_control: None,
        };
        let mut units = Vec::new();
        block_content_units(&block, "/result", &mut units);
        assert_eq!(
            pointers(&units),
            vec![("/result/content", ContentFormat::Text)]
        );

        let thinking = ContentBlock::Thinking {
            thinking: "reasoning".into(),
            signature: Some("sig".into()),
        };
        let mut units = Vec::new();
        block_content_units(&thinking, "/block", &mut units);
        assert_eq!(
            pointers(&units),
            vec![("/block/thinking", ContentFormat::Text)]
        );
    }

    #[test]
    fn non_body_blocks_and_metadata_are_not_units() {
        let item = item(json!({
            "role": "assistant",
            "content": [
                {"type": "tool_use", "id": "call", "name": "tool", "input": {"q": "x"}},
                {"type": "image", "source": {"type": "url", "url": "https://example.invalid/a.png"}},
                {"type": "redacted_thinking", "data": "opaque"},
            ],
            "meta": {"reference": "abcdefghijklmnopqrstuvwxyzab"}
        }));
        assert!(item_content_units(&item).is_empty());
    }

    #[test]
    fn format_values_are_the_persisted_encoding() {
        assert_eq!(ContentFormat::Text.as_byte(), 0);
        assert_eq!(ContentFormat::Json.as_byte(), 1);
        assert_eq!(ContentFormat::Text.as_database_value(), 0);
        assert_eq!(ContentFormat::Json.as_database_value(), 1);
        assert_eq!(
            ContentFormat::from_database_value(0),
            Some(ContentFormat::Text)
        );
        assert_eq!(
            ContentFormat::from_database_value(1),
            Some(ContentFormat::Json)
        );
        assert_eq!(ContentFormat::from_database_value(2), None);
    }

    #[test]
    fn role_is_irrelevant_to_units() {
        let item = item(json!({"role": Role::System, "content": ""}));
        let units = item_content_units(&item);
        assert_eq!(pointers(&units), vec![("/content", ContentFormat::Text)]);
        assert!(matches!(units[0].body, ContentUnitBody::Text("")));
    }
}
