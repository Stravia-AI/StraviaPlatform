use serde_json::json;
use stravia_runtime_contract::protocol::ir::request::ToolSpec;
use stravia_runtime_contract::protocol::ir::{
    AiItem, AiRequest, ContentBlock, MessageContent, Role,
};

/// OpenCode 2.4.2 免费层网关校验的最短可接受系统提示词（29 字节）：
/// 必须同时包含 `<env>`、`</env>` 以及位于其内部的大小写敏感 `Working directory:`。
pub(crate) const NATIVE_SYSTEM_PROMPT: &str = "<env>Working directory:</env>";

/// 与 `stravia-vendor-kiro` 一致的前置助手确认语，用于承接客户端原始系统/developer 指令。
pub(crate) const SYSTEM_PROMPT_ACK: &str = "I will follow these instructions.";

fn declaration(name: &str, parameters: serde_json::Value) -> ToolSpec {
    ToolSpec {
        name: name.into(),
        description: Some("Unavailable compatibility declaration; never call this tool.".into()),
        parameters,
        strict: Some(false),
        defer_loading: None,
    }
}

fn extract_instruction_text(item: &AiItem) -> Option<String> {
    if !matches!(item.role, Role::System | Role::Developer)
        || item.tool_calls.is_some()
        || item.tool_call_id.is_some()
    {
        return None;
    }
    match &item.content {
        MessageContent::Text(text) => Some(text.as_str().to_owned()),
        MessageContent::Blocks(blocks) => {
            let mut joined = String::new();
            for (index, block) in blocks.iter().enumerate() {
                let ContentBlock::Text { text, .. } = block else {
                    return None;
                };
                if index != 0 {
                    joined.push('\n');
                }
                joined.push_str(text.as_str());
            }
            Some(joined)
        }
    }
}

fn collect_client_instructions(request: &mut AiRequest) -> String {
    let mut collected = String::new();
    let mut push_segment = |segment: String| {
        if segment.trim().is_empty() {
            return;
        }
        if !collected.is_empty() {
            collected.push_str("\n\n");
        }
        collected.push_str(&segment);
    };

    if let Some(instructions) = request.instructions.take() {
        push_segment(instructions);
    }

    let mut drained = 0;
    while let Some(item) = request.items.get(drained) {
        let Some(text) = extract_instruction_text(item) else {
            break;
        };
        push_segment(text);
        drained += 1;
    }
    if drained > 0 {
        request.items.drain(..drained);
    }
    collected
}

/// 补齐免费层门控所需的出站工具声明与最短原生系统提示词；本插件没有工具执行能力。
/// 客户端原有的系统/developer 指令按 Kiro 插件方式移至首对 user/assistant 对话续接。
pub(crate) fn prepare(request: &mut AiRequest) {
    request.stream.enabled = true;
    // 官方 OpenCode 2.4.2 默认不发送 tool_choice，且免费层对非 auto 值返回 400；
    // 清除显式 tool_choice 以与官方线上请求体对齐并避免 400 拒绝。
    request.tool_choice = None;

    let tools = request.tools.get_or_insert_with(Vec::new);
    let missing_shell = !tools.iter().any(|tool| tool.name == "shell");
    let missing_read = !tools.iter().any(|tool| tool.name == "read");
    let missing_names = match (missing_shell, missing_read) {
        (true, true) => Some("shell`, `read"),
        (true, false) => Some("shell"),
        (false, true) => Some("read"),
        (false, false) => None,
    };
    if missing_shell {
        // 空 schema 无法满足免费层门控，保留 OpenCode 2.4.2 实际 shell 输入字段而不引入执行器。
        tools.push(declaration(
            "shell",
            json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string"},
                    "workdir": {"type": "string"},
                    "timeout": {
                        "type": "integer",
                        "minimum": 0
                    },
                    "background": {"type": "boolean"}
                },
                "required": ["command"],
                "additionalProperties": false
            }),
        ));
    }
    if missing_read {
        tools.push(declaration(
            "read",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "offset": {"type": "integer"},
                    "limit": {
                        "type": "integer",
                        "minimum": 0
                    }
                },
                "required": ["path"],
                "additionalProperties": false
            }),
        ));
    }

    let client_instructions = collect_client_instructions(request);

    let mut native_system = String::from(NATIVE_SYSTEM_PROMPT);
    if let Some(names) = missing_names {
        const NOTICE: &str = "\n\nThe following tools are compatibility declarations only, are unavailable in this environment, and must never be called: `";
        native_system.reserve(NOTICE.len() + names.len() + 2);
        native_system.push_str(NOTICE);
        native_system.push_str(names);
        native_system.push_str("`.");
    }
    request.instructions = Some(native_system);

    if !client_instructions.is_empty() {
        request.items.splice(
            0..0,
            [
                AiItem {
                    role: Role::User,
                    content: MessageContent::Text(client_instructions.into()),
                    tool_calls: None,
                    tool_call_id: None,
                    meta: None,
                },
                AiItem {
                    role: Role::Assistant,
                    content: MessageContent::Text(SYSTEM_PROMPT_ACK.to_owned().into()),
                    tool_calls: None,
                    tool_call_id: None,
                    meta: None,
                },
            ],
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use stravia_runtime_contract::protocol::ir::request::ToolChoice;
    use stravia_vendor_common::common;

    fn user_item(text: &str) -> AiItem {
        AiItem {
            role: Role::User,
            content: MessageContent::Text(text.to_owned().into()),
            tool_calls: None,
            tool_call_id: None,
            meta: None,
        }
    }

    fn text_of(item: &AiItem) -> &str {
        match &item.content {
            MessageContent::Text(text) => text.as_str(),
            other => panic!("expected text content, got {other:?}"),
        }
    }

    fn request() -> AiRequest {
        AiRequest::new("big-pickle", vec![user_item("Reply OK")])
    }

    fn client_tool(name: &str) -> ToolSpec {
        ToolSpec {
            name: name.into(),
            description: Some("Keep the client's business description.".into()),
            parameters: json!({
                "type": "object",
                "properties": {"customer": {"type": "string", "enum": ["Acme"]}},
                "required": ["customer"],
                "x-business": {"retention": 12}
            }),
            strict: Some(true),
            defer_loading: Some(true),
        }
    }

    #[test]
    fn client_definitions_survive_and_only_missing_names_are_forbidden() {
        for (provided, missing) in [("shell", "read"), ("read", "shell")] {
            let mut request = request();
            let original = vec![client_tool(provided), client_tool("business_tool")];
            request.tools = Some(original.clone());
            request.instructions = Some("  Preserve exactly.\n".into());
            prepare(&mut request);
            let tools = request.tools.as_ref().unwrap();
            assert_eq!(&tools[..original.len()], original.as_slice());
            assert_eq!(tools.len(), 3);
            assert_eq!(tools[2].name, missing);
            let instructions = request.instructions.as_deref().unwrap();
            assert!(instructions.starts_with(NATIVE_SYSTEM_PROMPT));
            assert!(instructions.contains(&format!("`{missing}`")));
            assert!(!instructions.contains(&format!("`{provided}`")));
            assert_eq!(request.items.len(), 3);
            assert_eq!(request.items[0].role, Role::User);
            assert_eq!(text_of(&request.items[0]), "  Preserve exactly.\n");
            assert_eq!(request.items[1].role, Role::Assistant);
            assert_eq!(text_of(&request.items[1]), SYSTEM_PROMPT_ACK);
            assert_eq!(request.items[2].role, Role::User);
            assert_eq!(text_of(&request.items[2]), "Reply OK");
        }
    }

    #[test]
    fn empty_or_blank_instructions_do_not_inject_ack_turn() {
        for original in [None, Some(""), Some(" \t\n")] {
            let mut request = request();
            request.instructions = original.map(str::to_owned);
            prepare(&mut request);
            let instructions = request.instructions.as_deref().unwrap();
            assert!(instructions.starts_with(NATIVE_SYSTEM_PROMPT));
            assert!(instructions.contains("`shell`"));
            assert!(instructions.contains("`read`"));
            assert_eq!(request.items.len(), 1);
            assert_eq!(request.items[0].role, Role::User);
            assert!(request.stream.enabled);
        }
    }

    #[test]
    fn complete_client_tools_keep_exact_minimal_native_system_prompt() {
        for (original, expected_chained) in [
            (None, None),
            (Some(""), None),
            (Some(" \t\n"), None),
            (Some("Do business work."), Some("Do business work.")),
        ] {
            let mut request = request();
            let original_tools = vec![client_tool("shell"), client_tool("read")];
            request.tools = Some(original_tools.clone());
            request.instructions = original.map(str::to_owned);
            prepare(&mut request);
            assert_eq!(request.tools.as_ref().unwrap(), &original_tools);
            assert_eq!(request.instructions.as_deref(), Some(NATIVE_SYSTEM_PROMPT));
            assert!(request.stream.enabled);
            match expected_chained {
                None => {
                    assert_eq!(request.items.len(), 1);
                    assert_eq!(text_of(&request.items[0]), "Reply OK");
                }
                Some(chained) => {
                    assert_eq!(request.items.len(), 3);
                    assert_eq!(request.items[0].role, Role::User);
                    assert_eq!(text_of(&request.items[0]), chained);
                    assert_eq!(request.items[1].role, Role::Assistant);
                    assert_eq!(text_of(&request.items[1]), SYSTEM_PROMPT_ACK);
                    assert_eq!(request.items[2].role, Role::User);
                    assert_eq!(text_of(&request.items[2]), "Reply OK");
                }
            }
        }
    }

    #[test]
    fn leading_system_and_developer_items_chain_with_top_level_instructions() {
        let mut request = AiRequest::new(
            "big-pickle",
            vec![
                AiItem {
                    role: Role::System,
                    content: MessageContent::Text("Chat system rule.".to_owned().into()),
                    tool_calls: None,
                    tool_call_id: None,
                    meta: None,
                },
                AiItem {
                    role: Role::Developer,
                    content: MessageContent::Blocks(vec![
                        ContentBlock::Text {
                            text: "Dev line 1.".to_owned().into(),
                            cache_control: None,
                        },
                        ContentBlock::Text {
                            text: "Dev line 2.".to_owned().into(),
                            cache_control: None,
                        },
                    ]),
                    tool_calls: None,
                    tool_call_id: None,
                    meta: None,
                },
                user_item("Actual question"),
            ],
        );
        request.instructions = Some("Top-level rule.".into());
        request.tools = Some(vec![client_tool("shell"), client_tool("read")]);
        prepare(&mut request);

        let encoded =
            common::encode_inference_request("openai-compatible/chat-completions/v1", request)
                .unwrap();
        let messages = encoded.body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 4);
        assert_eq!(messages[0]["role"], "system");
        assert_eq!(messages[0]["content"], NATIVE_SYSTEM_PROMPT);
        assert_eq!(messages[1]["role"], "user");
        assert_eq!(
            messages[1]["content"],
            "Top-level rule.\n\nChat system rule.\n\nDev line 1.\nDev line 2."
        );
        assert_eq!(messages[2]["role"], "assistant");
        assert_eq!(messages[2]["content"], SYSTEM_PROMPT_ACK);
        assert_eq!(messages[3]["role"], "user");
        assert_eq!(messages[3]["content"], "Actual question");
    }

    #[test]
    fn every_tool_choice_omits_wire_field_matching_v2_client() {
        let choices = [
            None,
            Some(ToolChoice::Auto),
            Some(ToolChoice::None),
            Some(ToolChoice::Required),
            Some(ToolChoice::Named {
                name: "shell".into(),
            }),
            Some(ToolChoice::Allowed {
                names: vec!["read".into()],
                required: true,
            }),
            Some(ToolChoice::Native(
                stravia_runtime_contract::protocol::ir::NativeToolKind::WebSearch,
            )),
        ];
        for choice in choices {
            for complete in [false, true] {
                let mut request = request();
                request.tool_choice = choice.clone();
                if complete {
                    request.tools = Some(vec![client_tool("shell"), client_tool("read")]);
                }
                prepare(&mut request);
                let encoded = common::encode_inference_request(
                    "openai-compatible/chat-completions/v1",
                    request,
                )
                .unwrap();
                assert!(encoded.body.get("tool_choice").is_none());
                assert_eq!(encoded.body["stream"], true);
            }
        }
    }

    #[test]
    fn injected_wire_schemas_accept_real_inputs_and_reject_invalid_inputs() {
        let mut request = request();
        prepare(&mut request);
        let encoded =
            common::encode_inference_request("openai-compatible/chat-completions/v1", request)
                .unwrap();
        let tools = encoded.body["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 2);
        let tool_fn = |name: &str| {
            &tools
                .iter()
                .find(|tool| tool["function"]["name"] == name)
                .unwrap()["function"]
        };
        assert_eq!(tool_fn("shell")["strict"], false);
        assert_eq!(tool_fn("read")["strict"], false);
        let shell =
            jsonschema::validator_for(&tool_fn("shell")["parameters"]).expect("valid shell schema");
        let read =
            jsonschema::validator_for(&tool_fn("read")["parameters"]).expect("valid read schema");
        for validator in [&shell, &read] {
            assert!(!validator.is_valid(&json!({})));
            assert!(!validator.is_valid(&json!([])));
        }
        for input in [
            json!({"command": "pwd"}),
            json!({"command": "pwd", "timeout": 0, "workdir": "/workspace", "background": true}),
            json!({"command": "pwd", "timeout": 120000, "background": false}),
        ] {
            assert!(shell.is_valid(&input), "{input}");
        }
        for input in [
            json!({"command": false}),
            json!({"command": "pwd", "timeout": -1}),
            json!({"command": "pwd", "timeout": 1.5}),
            json!({"command": "pwd", "workdir": 42}),
            json!({"command": "pwd", "background": "yes"}),
            json!({"command": "pwd", "extra": 1}),
        ] {
            assert!(!shell.is_valid(&input), "{input}");
        }
        for input in [
            json!({"path": "/workspace/input.txt"}),
            json!({"path": "/workspace/input.txt", "offset": 1, "limit": 0}),
            json!({"path": "/workspace/input.txt", "offset": 100, "limit": 2000}),
        ] {
            assert!(read.is_valid(&input), "{input}");
        }
        for input in [
            json!({"filePath": "/workspace/input.txt"}),
            json!({"path": 42}),
            json!({"path": "/workspace/input.txt", "offset": 1.5}),
            json!({"path": "/workspace/input.txt", "limit": -1}),
            json!({"path": "/workspace/input.txt", "limit": "1"}),
            json!({"path": "/workspace/input.txt", "extra": true}),
        ] {
            assert!(!read.is_valid(&input), "{input}");
        }
    }
}
