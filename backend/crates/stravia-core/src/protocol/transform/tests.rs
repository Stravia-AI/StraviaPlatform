// Contract cases in this module are adapted from anyllm_translate 0.16.0:
// https://github.com/whit3rabbit/anyllm-proxy/tree/75a5a3a230a3f26196cadecfa1a5378e804f2493/crates/translator/src/mapping
//
// MIT License
// Copyright (c) 2026
// Permission is hereby granted, free of charge, to any person obtaining a copy
// of this software and associated documentation files (the "Software"), to deal
// in the Software without restriction, including without limitation the rights
// to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
// copies of the Software, and to permit persons to whom the Software is
// furnished to do so, subject to the following conditions:
// The above copyright notice and this permission notice shall be included in all
// copies or substantial portions of the Software.
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
// IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
// FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
// AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
// LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
// OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
// SOFTWARE.
use serde_json::{Value, json};

use super::{
    ProtocolTransform, ThinkingCarrierFacts, TransformError, request_loss_paths,
    response_loss_paths, stream_loss_paths,
};
use crate::protocol::ids::{
    ANTHROPIC_MESSAGES_2023_06_01, GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA,
    OPEN_RESPONSES_2026_04_24, OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
};
use crate::protocol::ir::{
    AiItem, AiRequest, AiResponse, AiStreamDelta, ContentBlock, MediaSource, MessageContent, Role,
    ToolCall,
};

#[test]
fn thinking_carrier_facts_stay_behind_the_bound_protocol_pair() {
    let responses = ProtocolTransform::global()
        .bind(
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            OPEN_RESPONSES_2026_04_24,
        )
        .expect("registered protocol pair");
    assert_eq!(
        responses.thinking_carrier_facts(false),
        ThinkingCarrierFacts {
            indexed: true,
            may_be_protected: true,
            stream_unprotected_summaries: true,
        }
    );
    assert!(
        !responses
            .thinking_carrier_facts(true)
            .stream_unprotected_summaries
    );

    let anthropic = ProtocolTransform::global()
        .bind(
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            ANTHROPIC_MESSAGES_2023_06_01,
        )
        .expect("registered protocol pair");
    assert_eq!(
        anthropic.thinking_carrier_facts(false),
        ThinkingCarrierFacts {
            indexed: false,
            may_be_protected: true,
            stream_unprotected_summaries: false,
        }
    );
}

fn dated_response(id: &str, status: &str, output: Value, usage: Value) -> Value {
    json!({
        "id": id,
        "object": "response",
        "created_at": 1,
        "completed_at": 2,
        "status": status,
        "incomplete_details": (status == "incomplete")
            .then(|| json!({"reason": "max_output_tokens"})),
        "model": "gpt-5.4",
        "previous_response_id": null,
        "instructions": null,
        "output": output,
        "error": null,
        "tools": [],
        "tool_choice": "auto",
        "truncation": "disabled",
        "parallel_tool_calls": true,
        "text": {"format": {"type": "text"}},
        "top_p": null,
        "presence_penalty": null,
        "frequency_penalty": null,
        "top_logprobs": null,
        "temperature": null,
        "reasoning": null,
        "usage": usage,
        "max_output_tokens": null,
        "max_tool_calls": null,
        "store": false,
        "background": false,
        "service_tier": "default",
        "metadata": {},
        "safety_identifier": null,
        "prompt_cache_key": null
    })
}

#[test]
fn pair_bound_adapter_translates_openai_text_to_anthropic() {
    let pair = ProtocolTransform::global()
        .bind(
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            ANTHROPIC_MESSAGES_2023_06_01,
        )
        .expect("registered protocol pair");
    let request = pair
        .decode_request(json!({
            "model": "claude-sonnet-4-5",
            "messages": [{"role": "user", "content": "hello"}],
            "max_tokens": 128
        }))
        .expect("valid OpenAI request");

    assert_eq!(request.model, "claude-sonnet-4-5");
    let encoded = pair
        .encode_request(&request)
        .expect("representable Anthropic request");

    assert_eq!(encoded.path, "/v1/messages");
    assert_eq!(encoded.body["model"], "claude-sonnet-4-5");
    assert_eq!(encoded.body["messages"][0]["role"], "user");
    assert_eq!(encoded.body["messages"][0]["content"][0]["text"], "hello");
    assert_eq!(encoded.body["max_tokens"], 128);
    assert_eq!(encoded.headers["anthropic-version"], "2023-06-01");
}

#[test]
fn same_protocol_stream_does_not_apply_cross_protocol_loss_policy() {
    let pair = ProtocolTransform::global()
        .bind(
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
        )
        .expect("registered protocol pair");
    let (_, mut encoder) = pair.stream().expect("stream adapter").into_parts();

    encoder
        .encode_deltas(&[crate::protocol::ir::AiStreamDelta::Unknown {
            raw: "event: provider_metadata".to_string(),
        }])
        .expect("same-protocol stream must not apply cross-protocol loss policy");
}

#[test]
fn responses_stream_buffers_utf8_code_point_split_across_transport_chunks() {
    let pair = ProtocolTransform::global()
        .bind(OPEN_RESPONSES_2026_04_24, OPEN_RESPONSES_2026_04_24)
        .expect("registered protocol pair");
    let (mut decoder, _) = pair.stream().expect("stream-capable pair").into_parts();
    let created = crate::protocol::codec::open_responses::formatter::response_resource_snapshot(
        "resp_utf8",
        "grok-4.6",
        "in_progress",
        Vec::new(),
        Value::Null,
        Value::Null,
        Value::Null,
    );
    let event = |event: &str, sequence_number: u64, payload: Value| {
        let mut body = payload.as_object().expect("SSE payload object").clone();
        body.insert("type".into(), Value::String(event.to_owned()));
        body.insert("sequence_number".into(), sequence_number.into());
        format!("event: {event}\ndata: {}\n\n", Value::Object(body))
    };
    let upstream = [
        event("response.created", 0, json!({"response": created})),
        event(
            "response.output_item.added",
            1,
            json!({
                "output_index": 0,
                "item": {
                    "id": "msg_utf8",
                    "type": "message",
                    "status": "in_progress",
                    "role": "assistant",
                    "content": []
                }
            }),
        ),
        event(
            "response.content_part.added",
            2,
            json!({
                "output_index": 0,
                "item_id": "msg_utf8",
                "content_index": 0,
                "part": {
                    "type": "output_text",
                    "text": "",
                    "annotations": [],
                    "logprobs": []
                }
            }),
        ),
        event(
            "response.output_text.delta",
            3,
            json!({
                "output_index": 0,
                "item_id": "msg_utf8",
                "content_index": 0,
                "delta": "中文"
            }),
        ),
    ]
    .concat();
    let split = upstream.find("中文").expect("multibyte text") + 1;

    let mut deltas = decoder
        .decode_chunk(&upstream.as_bytes()[..split])
        .expect("an incomplete trailing code point must be buffered");
    deltas.extend(
        decoder
            .decode_chunk(&upstream.as_bytes()[split..])
            .expect("the next chunk must complete the code point"),
    );
    let text = deltas
        .into_iter()
        .filter_map(|delta| match delta {
            AiStreamDelta::TextDelta(text) => Some(text),
            AiStreamDelta::TextDeltaWithMetadata { text, .. } => Some(text),
            _ => None,
        })
        .collect::<String>();

    assert_eq!(text, "中文");
}

#[test]
fn responses_to_chat_omits_advisory_include_fields() {
    let pair = ProtocolTransform::global()
        .bind(
            OPEN_RESPONSES_2026_04_24,
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
        )
        .expect("registered protocol pair");
    let mut request = pair
        .decode_request(json!({
            "model": "chat-model",
            "input": "hello",
            "include": ["reasoning.encrypted_content"],
            "reasoning": {"effort": "high", "summary": "auto"},
            "client_metadata": {"session_id": "session"},
            "tools": [
                {
                    "type": "function",
                    "name": "shell_command",
                    "parameters": {"type": "object"}
                },
                {
                    "type": "namespace",
                    "name": "multi_agent_v1",
                    "tools": []
                },
                {"type": "web_search"}
            ],
            "tool_choice": "auto"
        }))
        .expect("valid Responses request");
    request.reasoning.target_control = Some(crate::thinking::TargetThinkingControl::Effort {
        value: "high".into(),
    });

    let encoded = pair
        .encode_request(&request)
        .expect("advisory response fields should not block a compatible provider");

    assert!(encoded.body.get("include").is_none());
    assert!(encoded.body.get("client_metadata").is_none());
    assert_eq!(encoded.body["reasoning_effort"], "high");
    assert_eq!(encoded.body["tools"].as_array().map(Vec::len), Some(1));
    assert_eq!(
        encoded.body["tools"][0]["function"]["name"],
        "shell_command"
    );
}

#[test]
fn responses_same_protocol_preserves_rolling_extensions() {
    let pair = ProtocolTransform::global()
        .bind(OPEN_RESPONSES_2026_04_24, OPEN_RESPONSES_2026_04_24)
        .expect("registered protocol pair");
    let request = pair
        .decode_request(json!({
            "model": "responses-model",
            "input": "hello",
            "client_metadata": {"session_id": "session"},
            "tools": [
                {
                    "type": "namespace",
                    "name": "multi_agent_v1",
                    "tools": []
                },
                {"type": "web_search"}
            ]
        }))
        .expect("rolling Responses request");

    let encoded = pair
        .encode_request(&request)
        .expect("same-protocol rolling extensions");

    assert_eq!(encoded.body["client_metadata"]["session_id"], "session");
    assert_eq!(encoded.body["tools"][0]["type"], "namespace");
    assert_eq!(encoded.body["tools"][1]["type"], "web_search");
}

#[test]
fn responses_to_chat_rejects_required_hosted_tools() {
    let pair = ProtocolTransform::global()
        .bind(
            OPEN_RESPONSES_2026_04_24,
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
        )
        .expect("registered protocol pair");
    let request = pair
        .decode_request(json!({
            "model": "chat-model",
            "input": "hello",
            "tools": [{"type": "web_search"}],
            "tool_choice": "required"
        }))
        .expect("rolling Responses request");

    let error = pair
        .encode_request(&request)
        .expect_err("required hosted tools are a hard constraint");

    assert!(matches!(
        error,
        TransformError::Unrepresentable { lost, .. } if lost == vec!["tools"]
    ));
}

#[test]
fn responses_rejects_unrepresentable_hard_tool_choices_before_provider_call() {
    let cases = [
        (ANTHROPIC_MESSAGES_2023_06_01, json!("none")),
        (GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA, json!("none")),
        (GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA, json!("required")),
        (
            GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA,
            json!({"type": "function", "name": "lookup"}),
        ),
    ];

    for (target, tool_choice) in cases {
        let pair = ProtocolTransform::global()
            .bind(OPEN_RESPONSES_2026_04_24, target)
            .expect("registered protocol pair");
        let request = pair
            .decode_request(json!({
                "model": "target-model",
                "input": "hello",
                "tools": [{"type": "function", "name": "lookup"}],
                "tool_choice": tool_choice,
            }))
            .expect("valid Responses request");

        let error = pair
            .encode_request(&request)
            .expect_err("hard tool choice must not be silently dropped");
        assert!(matches!(
            error,
            TransformError::Unrepresentable { ref lost, .. }
                if lost == &vec!["tool_choice".to_string()]
        ));
    }
}

#[test]
fn gemini_auto_tool_config_translates_to_openai_tool_choice() {
    let pair = ProtocolTransform::global()
        .bind(
            GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA,
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
        )
        .expect("registered protocol pair");
    let request = pair
        .decode_request(json!({
            "contents": [{
                "role": "user",
                "parts": [{"text": "hello"}]
            }],
            "tools": [{
                "functionDeclarations": [{
                    "name": "lookup",
                    "parameters": {"type": "object"}
                }]
            }],
            "toolConfig": {
                "functionCallingConfig": {"mode": "AUTO"}
            }
        }))
        .expect("valid Gemini request");

    let encoded = pair
        .encode_request(&request)
        .expect("Gemini AUTO tool selection is representable by OpenAI");

    assert_eq!(encoded.body["tool_choice"], "auto");
    assert_eq!(encoded.body["tools"][0]["function"]["name"], "lookup");
}

#[test]
fn gemini_constrained_auto_tool_config_remains_unrepresentable() {
    let pair = ProtocolTransform::global()
        .bind(
            GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA,
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
        )
        .expect("registered protocol pair");
    let request = pair
        .decode_request(json!({
            "contents": [{
                "role": "user",
                "parts": [{"text": "hello"}]
            }],
            "tools": [{
                "functionDeclarations": [
                    {"name": "lookup", "parameters": {"type": "object"}},
                    {"name": "search", "parameters": {"type": "object"}}
                ]
            }],
            "toolConfig": {
                "functionCallingConfig": {
                    "mode": "AUTO",
                    "allowedFunctionNames": ["lookup"]
                }
            }
        }))
        .expect("valid Gemini request");

    let error = pair
        .encode_request(&request)
        .expect_err("constrained Gemini tool selection must remain fail-closed");

    assert!(matches!(
        error,
        TransformError::Unrepresentable { ref lost, .. }
            if lost == &vec!["tool_config".to_string()]
    ));
}

#[test]
fn responses_cross_protocol_distinguishes_advisory_and_hard_controls() {
    let pair = ProtocolTransform::global()
        .bind(
            OPEN_RESPONSES_2026_04_24,
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
        )
        .expect("registered protocol pair");
    let request = pair
        .decode_request(json!({
            "model": "chat-model",
            "input": "hello",
            "prompt_cache_key": "cache-key",
            "service_tier": "default",
            "metadata": {"tenant": "local"},
            "safety_identifier": "local-user",
            "store": false,
            "top_logprobs": 3,
            "include": ["reasoning.encrypted_content"]
        }))
        .expect("valid Responses request");
    pair.encode_request(&request)
        .expect("advisory controls may stay local");

    for (field, value) in [("max_tool_calls", json!(2)), ("truncation", json!("auto"))] {
        let mut body = json!({
            "model": "chat-model",
            "input": "hello"
        });
        body[field] = value;
        let request = pair.decode_request(body).expect("valid Responses request");
        let error = pair
            .encode_request(&request)
            .expect_err("hard control must not be silently dropped");
        assert!(matches!(
            error,
            TransformError::Unrepresentable { lost, .. } if lost == vec![field]
        ));
    }
}

#[test]
fn pair_bound_adapter_rejects_cross_protocol_candidate_loss() {
    let pair = ProtocolTransform::global()
        .bind(
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            ANTHROPIC_MESSAGES_2023_06_01,
        )
        .expect("registered protocol pair");
    let request = pair
        .decode_request(json!({
            "model": "claude-sonnet-4-5",
            "messages": [{"role": "user", "content": "compare"}],
            "n": 2
        }))
        .expect("valid OpenAI request");

    let error = pair
        .encode_request(&request)
        .expect_err("Anthropic cannot represent multiple candidates");

    match error {
        TransformError::Unrepresentable {
            ingress,
            egress,
            direction,
            lost,
        } => {
            assert_eq!(ingress, OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1);
            assert_eq!(egress, ANTHROPIC_MESSAGES_2023_06_01);
            assert_eq!(direction, "client_request");
            assert_eq!(lost, ["n"]);
        }
        other => panic!("expected typed representability error, got {other:?}"),
    }
}

#[test]
fn donor_anthropic_system_blocks_map_to_gemini_system_instruction() {
    let pair = ProtocolTransform::global()
        .bind(
            ANTHROPIC_MESSAGES_2023_06_01,
            GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA,
        )
        .expect("registered protocol pair");
    let request = pair
        .decode_request(json!({
            "model": "gemini-2.5-pro",
            "max_tokens": 1024,
            "system": [
                {"type": "text", "text": "First."},
                {"type": "text", "text": "Second."}
            ],
            "messages": [{"role": "user", "content": "Hi"}]
        }))
        .expect("valid Anthropic request");

    let encoded = pair
        .encode_request(&request)
        .expect("representable Gemini request");

    assert_eq!(
        encoded.body["systemInstruction"]["parts"][0]["text"],
        "First.\nSecond."
    );
    assert_eq!(encoded.body["contents"][0]["role"], "user");
    assert_eq!(encoded.body["contents"][0]["parts"][0]["text"], "Hi");
}

#[test]
fn donor_gemini_system_instruction_maps_to_responses_instructions() {
    let pair = ProtocolTransform::global()
        .bind(
            GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA,
            OPEN_RESPONSES_2026_04_24,
        )
        .expect("registered protocol pair");
    let request = pair
        .decode_request(json!({
            "systemInstruction": {
                "parts": [{"text": "You are OpenCode."}]
            },
            "contents": [{
                "role": "user",
                "parts": [{"text": "Inspect the repository."}]
            }]
        }))
        .expect("valid Gemini request");

    let encoded = pair
        .encode_request(&request)
        .expect("representable Responses request");

    assert_eq!(encoded.body["instructions"], "You are OpenCode.");
    assert_eq!(encoded.body["input"].as_array().map(Vec::len), Some(1));
    assert_eq!(encoded.body["input"][0]["role"], "user");
}

#[test]
fn donor_anthropic_base64_image_maps_to_gemini_inline_data() {
    let pair = ProtocolTransform::global()
        .bind(
            ANTHROPIC_MESSAGES_2023_06_01,
            GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA,
        )
        .expect("registered protocol pair");
    let request = pair
        .decode_request(json!({
            "model": "gemini-2.5-pro",
            "max_tokens": 1024,
            "messages": [{
                "role": "user",
                "content": [{
                    "type": "image",
                    "source": {
                        "type": "base64",
                        "media_type": "image/jpeg",
                        "data": "abc123=="
                    }
                }]
            }]
        }))
        .expect("valid Anthropic image request");

    let encoded = pair
        .encode_request(&request)
        .expect("representable Gemini image request");

    let inline_data = &encoded.body["contents"][0]["parts"][0]["inlineData"];
    assert_eq!(inline_data["mimeType"], "image/jpeg");
    assert_eq!(inline_data["data"], "abc123==");
}

#[test]
fn donor_anthropic_tool_turn_maps_to_responses_items() {
    let pair = ProtocolTransform::global()
        .bind(ANTHROPIC_MESSAGES_2023_06_01, OPEN_RESPONSES_2026_04_24)
        .expect("registered protocol pair");
    let request = pair
        .decode_request(json!({
            "model": "gpt-5.4",
            "max_tokens": 100,
            "messages": [
                {"role": "user", "content": "Weather?"},
                {"role": "assistant", "content": [{
                    "type": "tool_use",
                    "id": "toolu_123",
                    "name": "get_weather",
                    "input": {"city": "NYC"}
                }]},
                {"role": "user", "content": [{
                    "type": "tool_result",
                    "tool_use_id": "toolu_123",
                    "content": "72F sunny"
                }]}
            ]
        }))
        .expect("valid Anthropic tool turn");

    let encoded = pair
        .encode_request(&request)
        .expect("representable Responses tool turn");
    let items = encoded.body["input"].as_array().expect("Responses items");

    assert_eq!(items.len(), 3);
    assert_eq!(items[0]["type"], "message");
    assert_eq!(items[1]["type"], "function_call");
    assert_eq!(items[1]["call_id"], "toolu_123");
    assert_eq!(items[1]["name"], "get_weather");
    assert_eq!(items[1]["arguments"], r#"{"city":"NYC"}"#);
    assert_eq!(items[2]["type"], "function_call_output");
    assert_eq!(items[2]["call_id"], "toolu_123");
    assert_eq!(items[2]["output"], "72F sunny");
}

#[test]
fn donor_anthropic_system_maps_to_responses_instructions() {
    let pair = ProtocolTransform::global()
        .bind(ANTHROPIC_MESSAGES_2023_06_01, OPEN_RESPONSES_2026_04_24)
        .expect("registered protocol pair");
    let request = pair
        .decode_request(json!({
            "model": "gpt-5.6-sol",
            "max_tokens": 1024,
            "system": "You are a careful coding assistant.",
            "messages": [
                {"role": "user", "content": "Test AC-0"}
            ]
        }))
        .expect("valid Anthropic request");

    let encoded = pair
        .encode_request(&request)
        .expect("representable Responses request");

    assert_eq!(
        encoded.body["instructions"],
        "You are a careful coding assistant."
    );
    assert_eq!(encoded.body["input"].as_array().map(Vec::len), Some(1));
    assert_eq!(encoded.body["input"][0]["role"], "user");
}

#[test]
fn donor_responses_multimodal_function_output_maps_to_anthropic_tool_result() {
    let pair = ProtocolTransform::global()
        .bind(OPEN_RESPONSES_2026_04_24, ANTHROPIC_MESSAGES_2023_06_01)
        .expect("registered protocol pair");
    let request = pair
        .decode_request(json!({
            "model": "claude-sonnet",
            "input": [{
                "type": "function_call_output",
                "call_id": "call_123",
                "output": [
                    {"type": "input_text", "text": "chart"},
                    {"type": "input_image", "image_url": "https://example.test/chart.png"}
                ]
            }],
            "max_output_tokens": 128
        }))
        .expect("valid Responses function output");

    let encoded = pair
        .encode_request(&request)
        .expect("representable Anthropic tool result");
    let result = &encoded.body["messages"][0]["content"][0];

    assert_eq!(result["type"], "tool_result");
    assert_eq!(result["tool_use_id"], "toolu_call_123");
    assert_eq!(result["content"][0]["type"], "text");
    assert_eq!(result["content"][1]["type"], "image");
}

#[test]
fn donor_responses_multimodal_function_output_maps_to_google_function_response() {
    let pair = ProtocolTransform::global()
        .bind(
            OPEN_RESPONSES_2026_04_24,
            GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA,
        )
        .expect("registered protocol pair");
    let request = pair
        .decode_request(json!({
            "model": "gemini-2.5-pro",
            "input": [
                {
                    "type": "function_call",
                    "call_id": "get_chart",
                    "name": "render_chart",
                    "arguments": "{}"
                },
                {
                    "type": "function_call_output",
                    "call_id": "get_chart",
                    "output": [
                        {"type": "input_text", "text": "chart"},
                        {"type": "input_file", "file_url": "https://example.test/chart.pdf"}
                    ]
                }
            ],
            "max_output_tokens": 128
        }))
        .expect("valid Responses function output");

    let encoded = pair
        .encode_request(&request)
        .expect("representable Google function response");
    let parts = encoded.body["contents"][1]["parts"]
        .as_array()
        .expect("Google parts");

    assert_eq!(parts[0]["functionResponse"]["name"], "render_chart");
    assert_eq!(
        parts[0]["functionResponse"]["parts"][0]["fileData"]["fileUri"],
        "https://example.test/chart.pdf"
    );
}
#[test]
fn donor_responses_function_call_maps_to_anthropic_tool_use() {
    let pair = ProtocolTransform::global()
        .bind(ANTHROPIC_MESSAGES_2023_06_01, OPEN_RESPONSES_2026_04_24)
        .expect("registered protocol pair");
    let response = pair
        .decode_response(dated_response(
            "resp_abc",
            "completed",
            json!([{
                "type": "function_call",
                "id": "fc_1",
                "call_id": "call_123",
                "name": "get_weather",
                "arguments": "{\"city\":\"NYC\"}",
                "status": "completed"
            }]),
            json!({
                "input_tokens": 20,
                "output_tokens": 15,
                "total_tokens": 35,
                "input_tokens_details": {"cached_tokens": 0},
                "output_tokens_details": {"reasoning_tokens": 0}
            }),
        ))
        .expect("valid Responses function call");
    assert_eq!(response.tool_calls().count(), 1);
    assert_eq!(response.items.len(), 1);

    let encoded = pair
        .encode_response(&response)
        .expect("representable Anthropic response");

    assert_eq!(encoded["content"][0]["type"], "tool_use");
    assert_eq!(encoded["content"][0]["id"], "call_123");
    assert_eq!(encoded["content"][0]["name"], "get_weather");
    assert_eq!(encoded["content"][0]["input"]["city"], "NYC");
    assert_eq!(encoded["stop_reason"], "tool_use");
    assert_eq!(encoded["usage"]["input_tokens"], 20);
    assert_eq!(encoded["usage"]["output_tokens"], 15);
}

#[test]
fn donor_responses_incomplete_status_maps_to_anthropic_max_tokens() {
    let pair = ProtocolTransform::global()
        .bind(ANTHROPIC_MESSAGES_2023_06_01, OPEN_RESPONSES_2026_04_24)
        .expect("registered protocol pair");
    let response = pair
        .decode_response(dated_response(
            "resp_partial",
            "incomplete",
            json!([{
                "type": "message",
                "role": "assistant",
                "content": [{"type": "output_text", "text": "partial...", "annotations": []}],
                "id": "msg_1",
                "status": "incomplete"
            }]),
            json!({
                "input_tokens": 10,
                "output_tokens": 100,
                "total_tokens": 110,
                "input_tokens_details": {"cached_tokens": 0},
                "output_tokens_details": {"reasoning_tokens": 0}
            }),
        ))
        .expect("valid incomplete Responses response");

    let encoded = pair
        .encode_response(&response)
        .expect("representable Anthropic response");

    assert_eq!(encoded["content"][0]["text"], "partial...");
    assert_eq!(encoded["stop_reason"], "max_tokens");
}

#[test]
fn donor_responses_incomplete_stream_maps_to_anthropic_max_tokens() {
    let pair = ProtocolTransform::global()
        .bind(ANTHROPIC_MESSAGES_2023_06_01, OPEN_RESPONSES_2026_04_24)
        .expect("registered protocol pair");
    let (mut decoder, mut encoder) = pair.stream().expect("stream-capable pair").into_parts();
    let output_item = json!({
        "id": "msg_1",
        "type": "message",
        "status": "incomplete",
        "role": "assistant",
        "content": [{"type": "output_text", "text": "partial...", "annotations": []}]
    });
    let usage = json!({
        "input_tokens": 10,
        "output_tokens": 100,
        "total_tokens": 110,
        "input_tokens_details": {"cached_tokens": 0},
        "output_tokens_details": {"reasoning_tokens": 0}
    });
    let created = crate::protocol::codec::open_responses::formatter::response_resource_snapshot(
        "resp_partial",
        "gpt-5.4",
        "in_progress",
        Vec::new(),
        Value::Null,
        Value::Null,
        Value::Null,
    );
    let incomplete = crate::protocol::codec::open_responses::formatter::response_resource_snapshot(
        "resp_partial",
        "gpt-5.4",
        "incomplete",
        vec![output_item.clone()],
        json!({"reason": "max_output_tokens"}),
        Value::Null,
        usage,
    );
    let event = |event: &str, sequence_number: u64, payload: Value| {
        let mut body = payload.as_object().expect("SSE payload object").clone();
        body.insert("type".into(), Value::String(event.to_owned()));
        body.insert("sequence_number".into(), sequence_number.into());
        format!("event: {event}\ndata: {}\n\n", Value::Object(body))
    };
    let upstream = [
            event("response.created", 0, json!({"response": created})),
            event(
                "response.output_item.added",
                1,
                json!({"output_index": 0, "item": {"id": "msg_1", "type": "message", "status": "in_progress", "role": "assistant", "content": []}}),
            ),
            event(
                "response.content_part.added",
                2,
                json!({"output_index": 0, "item_id": "msg_1", "content_index": 0, "part": {"type": "output_text", "text": "", "annotations": [], "logprobs": []}}),
            ),
            event(
                "response.output_text.delta",
                3,
                json!({"output_index": 0, "item_id": "msg_1", "content_index": 0, "delta": "partial..."}),
            ),
            event(
                "response.content_part.done",
                4,
                json!({"output_index": 0, "item_id": "msg_1", "content_index": 0, "part": {"type": "output_text", "text": "partial...", "annotations": [], "logprobs": []}}),
            ),
            event(
                "response.output_item.done",
                5,
                json!({"output_index": 0, "item": output_item}),
            ),
            event(
                "response.incomplete",
                6,
                json!({"response": incomplete}),
            ),
        ]
        .concat();

    let deltas = decoder
        .decode_chunk(upstream.as_bytes())
        .expect("valid Responses stream");
    let events = encoder
        .encode_deltas(&deltas)
        .expect("representable Anthropic stream");
    let terminal = events
        .iter()
        .find(|event| event.event.as_deref() == Some("message_delta"))
        .expect("Anthropic terminal delta");
    let terminal: Value = serde_json::from_str(&terminal.data).expect("valid event JSON");

    assert_eq!(terminal["delta"]["stop_reason"], "max_tokens");
    assert_eq!(terminal["usage"]["output_tokens"], 100);
}

#[test]
fn donor_responses_reasoning_summary_stream_maps_to_anthropic_thinking() {
    let pair = ProtocolTransform::global()
        .bind(ANTHROPIC_MESSAGES_2023_06_01, OPEN_RESPONSES_2026_04_24)
        .expect("registered protocol pair");
    let (mut decoder, mut encoder) = pair.stream().expect("stream-capable pair").into_parts();
    let created = crate::protocol::codec::open_responses::formatter::response_resource_snapshot(
        "resp_reasoning",
        "gpt-5.6",
        "in_progress",
        Vec::new(),
        Value::Null,
        Value::Null,
        Value::Null,
    );
    let event = |event: &str, sequence_number: u64, payload: Value| {
        let mut body = payload.as_object().expect("SSE payload object").clone();
        body.insert("type".into(), Value::String(event.to_owned()));
        body.insert("sequence_number".into(), sequence_number.into());
        format!("event: {event}\ndata: {}\n\n", Value::Object(body))
    };
    let upstream = [
        event("response.created", 0, json!({"response": created})),
        event(
            "response.output_item.added",
            1,
            json!({
                "output_index": 0,
                "item": {
                    "id": "rs_1",
                    "type": "reasoning",
                    "content": [],
                    "summary": []
                }
            }),
        ),
        event(
            "response.reasoning_summary_part.added",
            2,
            json!({
                "item_id": "rs_1",
                "output_index": 0,
                "summary_index": 0,
                "part": {"type": "summary_text", "text": ""}
            }),
        ),
        event(
            "response.reasoning_summary_text.delta",
            3,
            json!({
                "item_id": "rs_1",
                "output_index": 0,
                "summary_index": 0,
                "delta": "**Inspecting file tree with glob**"
            }),
        ),
    ]
    .concat();

    let deltas = decoder
        .decode_chunk(upstream.as_bytes())
        .expect("valid Responses reasoning summary stream");
    let events = encoder
        .encode_deltas(&deltas)
        .expect("reasoning summary is representable as Anthropic thinking");
    let thinking = events
        .iter()
        .find(|event| event.data.contains("\"type\":\"thinking_delta\""))
        .expect("Anthropic thinking delta");
    let thinking: Value = serde_json::from_str(&thinking.data).expect("valid event JSON");

    assert_eq!(
        thinking["delta"]["thinking"],
        "**Inspecting file tree with glob**"
    );
}

#[test]
fn canonical_video_is_native_for_gemini_and_rejected_for_anthropic() {
    let request = AiRequest::new(
        "gemini-video",
        vec![AiItem {
            role: Role::User,
            content: MessageContent::Blocks(vec![ContentBlock::Video {
                source: MediaSource::Url("https://example.com/video.mp4".into()),
                media_type: Some("video/mp4".into()),
            }]),
            tool_calls: None,
            tool_call_id: None,
            meta: None,
        }],
    );
    let gemini = ProtocolTransform::global()
        .bind(
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA,
        )
        .expect("Gemini protocol pair")
        .encode_request(&request)
        .expect("Gemini native video");
    assert_eq!(
        gemini.body["contents"][0]["parts"][0]["fileData"]["mimeType"],
        "video/mp4"
    );

    let error = ProtocolTransform::global()
        .bind(
            GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA,
            ANTHROPIC_MESSAGES_2023_06_01,
        )
        .expect("Anthropic protocol pair")
        .encode_request(&request)
        .expect_err("Anthropic must reject native video");
    assert!(matches!(
        error,
        TransformError::Unrepresentable { lost, .. }
            if lost == vec!["messages[0].content[0]"]
    ));
}
#[test]
fn responses_reasoning_before_tool_call_is_preserved_for_openai_compatible() {
    let request = AiRequest::new(
        "reasoning-model",
        vec![
            AiItem {
                role: Role::User,
                content: MessageContent::Text("inspect".into()),
                tool_calls: None,
                tool_call_id: None,
                meta: None,
            },
            AiItem {
                role: Role::Assistant,
                content: MessageContent::Blocks(vec![ContentBlock::Thinking {
                    thinking: "inspect the image".into(),
                    signature: None,
                }]),
                tool_calls: Some(vec![ToolCall {
                    id: "call_media".into(),
                    name: "understand_media".into(),
                    arguments: "{}".into(),
                }]),
                tool_call_id: None,
                meta: None,
            },
            AiItem {
                role: Role::Tool,
                content: MessageContent::Text("{\"completion\":\"complete\"}".into()),
                tool_calls: None,
                tool_call_id: Some("call_media".into()),
                meta: None,
            },
        ],
    );

    let encoded = ProtocolTransform::global()
        .bind(
            OPEN_RESPONSES_2026_04_24,
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
        )
        .expect("registered protocol pair")
        .encode_request(&request)
        .expect("reasoning-only assistant tool call is representable");

    assert_eq!(
        encoded.body["messages"][1]["reasoning_content"],
        "inspect the image"
    );
    assert_eq!(
        encoded.body["messages"][1]["tool_calls"][0]["function"]["name"],
        "understand_media"
    );
}

#[test]
fn responses_replay_preserves_canonical_reasoning_item() {
    let request = AiRequest::new(
        "reasoning-model",
        vec![AiItem::reasoning(
            vec!["summary".into()],
            Vec::new(),
            Some("opaque".into()),
        )],
    );

    let encoded = ProtocolTransform::global()
        .bind(ANTHROPIC_MESSAGES_2023_06_01, OPEN_RESPONSES_2026_04_24)
        .expect("registered protocol pair")
        .encode_request(&request)
        .expect("canonical reasoning is native Open Responses input");

    assert_eq!(encoded.body["input"][0]["type"], "reasoning");
    assert_eq!(encoded.body["input"][0]["summary"][0]["text"], "summary");
    assert_eq!(encoded.body["input"][0]["encrypted_content"], "opaque");
}

#[test]
fn strict_function_schema_is_rejected_when_target_cannot_preserve_it() {
    let source = ProtocolTransform::global()
        .bind(OPEN_RESPONSES_2026_04_24, OPEN_RESPONSES_2026_04_24)
        .expect("registered source pair");
    let request = source
        .decode_request(json!({
            "model": "logical-model",
            "input": "hello",
            "tools": [{
                "type": "function",
                "name": "lookup",
                "strict": true,
                "parameters": {
                    "type": "object",
                    "properties": {"query": {"type": "string"}},
                    "additionalProperties": false
                }
            }]
        }))
        .expect("valid strict tool");

    for target in [
        ANTHROPIC_MESSAGES_2023_06_01,
        GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA,
    ] {
        let error = ProtocolTransform::global()
            .bind(OPEN_RESPONSES_2026_04_24, target)
            .expect("registered target pair")
            .encode_request(&request)
            .expect_err("target must reject strict schema loss");
        assert!(
            matches!(
                error,
                TransformError::Unrepresentable { ref lost, .. }
                    if lost.iter().any(|path| path == "tools[0].strict")
            ),
            "unexpected transform error: {error}"
        );
    }
}
#[test]
fn refusal_semantics_fail_closed_for_protocols_without_refusal_items() {
    let pair = ProtocolTransform::global()
        .bind(
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            OPEN_RESPONSES_2026_04_24,
        )
        .expect("registered protocol pair");
    let mut response = AiResponse::new("resp-refusal", "logical-model");
    let mut mixed = AiItem::output_text("visible");
    mixed.content = MessageContent::Blocks(vec![
        ContentBlock::Text {
            text: "visible".into(),
            cache_control: None,
        },
        ContentBlock::Refusal {
            refusal: "cannot comply".into(),
        },
    ]);
    response.items.push(mixed);

    assert_eq!(
        response_loss_paths(pair, &response),
        vec!["items[0].refusal"]
    );
    assert_eq!(
        stream_loss_paths(pair, &[AiStreamDelta::RefusalDelta("cannot comply".into())]),
        vec!["deltas[0].refusal"]
    );
}

#[test]
fn response_annotations_and_logprobs_fail_closed_outside_open_responses() {
    let pair = ProtocolTransform::global()
        .bind(
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            OPEN_RESPONSES_2026_04_24,
        )
        .expect("registered protocol pair");
    let mut item = AiItem::output_text("answer");
    item.meta = Some(json!({
        "__open_responses_content": [{
            "type": "output_text",
            "text": "answer",
            "annotations": [{"type": "url_citation", "url": "https://example.test"}],
            "logprobs": [{"token": "answer", "logprob": -0.1}]
        }]
    }));
    let mut response = AiResponse::new("resp-metadata", "logical-model");
    response.items.push(item);

    assert_eq!(
        response_loss_paths(pair, &response),
        vec!["items[0].annotations", "items[0].logprobs"]
    );
}

#[test]
fn dated_stream_metadata_rejects_semantics_but_allows_padding() {
    let pair = ProtocolTransform::global()
        .bind(
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            OPEN_RESPONSES_2026_04_24,
        )
        .expect("registered protocol pair");

    assert_eq!(
        stream_loss_paths(
            pair,
            &[
                AiStreamDelta::TextDeltaWithMetadata {
                    text: "answer".into(),
                    logprobs: vec![json!({"token": "answer"})],
                    obfuscation: Some("pad".into()),
                    output_index: None,
                    content_index: None,
                },
                AiStreamDelta::ReasoningSummaryDelta {
                    text: "summary".into(),
                    obfuscation: None,
                    output_index: None,
                    content_index: None,
                },
            ],
        ),
        vec!["deltas[0].logprobs"]
    );
}

#[test]
fn openai_compatible_projects_reasoning_without_losing_server_continuation_state() {
    let pair = ProtocolTransform::global()
        .bind(
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            OPEN_RESPONSES_2026_04_24,
        )
        .expect("registered protocol pair");
    let mut response = AiResponse::new("resp-reasoning", "logical-model");
    response.items = vec![
        AiItem::reasoning(
            vec!["visible ".into()],
            vec!["reasoning".into()],
            Some("opaque".into()),
        ),
        AiItem::output_text("answer"),
    ];

    assert!(response_loss_paths(pair, &response).is_empty());
    assert!(
        stream_loss_paths(
            pair,
            &[
                AiStreamDelta::ReasoningSummaryDelta {
                    text: "visible reasoning".into(),
                    obfuscation: None,
                    output_index: Some(0),
                    content_index: Some(0),
                },
                AiStreamDelta::ThinkingSignature("opaque".into()),
            ],
        )
        .is_empty()
    );

    let encoded = pair
        .encode_response(&response)
        .expect("OpenAI-compatible reasoning projection");
    assert_eq!(
        encoded["choices"][0]["message"]["reasoning_content"],
        "visible reasoning"
    );
    assert_eq!(encoded["choices"][0]["message"]["content"], "answer");
    assert!(
        encoded["choices"][0]["message"]
            .get("encrypted_content")
            .is_none()
    );
}

#[test]
fn gemini_stream_accepts_responses_reasoning_summary() {
    let pair = ProtocolTransform::global()
        .bind(
            GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA,
            OPEN_RESPONSES_2026_04_24,
        )
        .expect("registered protocol pair");

    assert!(
        stream_loss_paths(
            pair,
            &[AiStreamDelta::ReasoningSummaryDelta {
                text: "summary".into(),
                obfuscation: None,
                output_index: Some(0),
                content_index: Some(0),
            }],
        )
        .is_empty()
    );
}
#[test]
fn cross_protocol_omits_advisory_text_and_stream_controls() {
    let pair = ProtocolTransform::global()
        .bind(OPEN_RESPONSES_2026_04_24, ANTHROPIC_MESSAGES_2023_06_01)
        .expect("registered protocol pair");
    let request = pair
        .decode_request(json!({
            "model": "logical-model",
            "input": "hello",
            "stream_options": {"include_obfuscation": true},
            "text": {
                "format": {"type": "text"},
                "verbosity": "high"
            }
        }))
        .expect("dated request");

    assert!(request_loss_paths(pair, &request).is_empty());
}
#[test]
fn open_responses_client_rejects_unknown_provider_output_items() {
    let pair = ProtocolTransform::global()
        .bind(
            OPEN_RESPONSES_2026_04_24,
            GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA,
        )
        .expect("registered protocol pair");
    let mut response = AiResponse::new("response", "model");
    response.items.push(AiItem::unknown(
        json!({"type": "executable_code", "code": "1 + 1"}),
    ));

    assert_eq!(response_loss_paths(pair, &response), vec!["items[0]"]);
}

#[test]
fn image_detail_fails_closed_for_targets_without_an_equivalent_control() {
    let pair = ProtocolTransform::global()
        .bind(OPEN_RESPONSES_2026_04_24, ANTHROPIC_MESSAGES_2023_06_01)
        .expect("registered protocol pair");
    let request = pair
        .decode_request(json!({
            "model": "model",
            "input": [{
                "role": "user",
                "content": [{
                    "type": "input_image",
                    "image_url": "https://example.test/image.png",
                    "detail": "high"
                }]
            }]
        }))
        .expect("dated request");

    assert!(
        request_loss_paths(pair, &request)
            .iter()
            .any(|path| path == "messages[0].content[0].detail")
    );
}
#[test]
fn google_structured_output_is_representable_for_open_responses_requests() {
    let pair = ProtocolTransform::global()
        .bind(
            OPEN_RESPONSES_2026_04_24,
            GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA,
        )
        .expect("registered protocol pair");
    let request = pair
        .decode_request(json!({
            "model": "model",
            "input": "hello",
            "text": {
                "format": {
                    "type": "json_schema",
                    "name": "answer",
                    "strict": true,
                    "schema": {
                        "type": "object",
                        "properties": {"answer": {"type": "string"}}
                    }
                }
            }
        }))
        .expect("dated request");

    assert!(!request_loss_paths(pair, &request).contains(&"text".to_string()));
    let encoded = pair
        .encode_request(&request)
        .expect("lossless Gemini structured output");
    assert_eq!(
        encoded.body["generationConfig"]["responseMimeType"],
        "application/json"
    );
    assert_eq!(
        encoded.body["generationConfig"]["responseSchema"]["properties"]["answer"]["type"],
        "string"
    );

    let lossy = pair
        .decode_request(json!({
            "model": "model",
            "input": "hello",
            "text": {
                "format": {
                    "type": "json_schema",
                    "name": "answer",
                    "strict": true,
                    "schema": {
                        "type": "object",
                        "additionalProperties": false,
                        "properties": {"answer": {"type": "string"}}
                    }
                }
            }
        }))
        .expect("dated request");
    assert!(request_loss_paths(pair, &lossy).contains(&"text".to_string()));
}

#[test]
fn open_responses_client_rejects_assistant_image_output() {
    let pair = ProtocolTransform::global()
        .bind(
            OPEN_RESPONSES_2026_04_24,
            GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA,
        )
        .expect("registered protocol pair");
    let mut response = AiResponse::new("response", "model");
    response.items.push(AiItem {
        role: Role::Assistant,
        content: MessageContent::Blocks(vec![ContentBlock::Image {
            source: crate::protocol::ir::MediaSource::Url("https://example.test/image.png".into()),
            detail: None,
            cache_control: None,
        }]),
        tool_calls: None,
        tool_call_id: None,
        meta: None,
    });

    assert_eq!(response_loss_paths(pair, &response), vec!["items[0]"]);
}

#[test]
fn open_responses_client_rejects_unrepresentable_tool_output_blocks() {
    let pair = ProtocolTransform::global()
        .bind(
            OPEN_RESPONSES_2026_04_24,
            GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA,
        )
        .expect("registered protocol pair");
    let mut response = AiResponse::new("response", "model");
    response.items.push(AiItem {
        role: Role::Tool,
        content: MessageContent::Blocks(vec![ContentBlock::Audio {
            source: crate::protocol::ir::MediaSource::Base64 {
                media_type: "audio/wav".into(),
                data: "aGk=".into(),
            },
        }]),
        tool_calls: None,
        tool_call_id: Some("call_1".into()),
        meta: None,
    });
    response.items.push(AiItem {
        role: Role::Tool,
        content: MessageContent::Blocks(vec![ContentBlock::Image {
            source: crate::protocol::ir::MediaSource::Url("https://example.test/image.png".into()),
            detail: Some("future".into()),
            cache_control: None,
        }]),
        tool_calls: None,
        tool_call_id: Some("call_2".into()),
        meta: None,
    });

    assert_eq!(
        response_loss_paths(pair, &response),
        vec!["items[0]", "items[1]"]
    );
}

#[test]
fn canonical_google_stream_metadata_is_advisory_but_unknown_items_are_not() {
    let pair = ProtocolTransform::global()
        .bind(
            OPEN_RESPONSES_2026_04_24,
            GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA,
        )
        .expect("registered protocol pair");
    let metadata = AiStreamDelta::Unknown {
        raw: json!({
            "__google_response_metadata": {
                "modelVersion": "gemini-2",
                "responseId": "response-1",
                "usageMetadata": {"promptTokenCount": 1}
            }
        })
        .to_string(),
    };
    let unknown_item = AiStreamDelta::Unknown {
        raw: json!({"type": "executable_code", "code": "1 + 1"}).to_string(),
    };

    assert!(stream_loss_paths(pair, &[metadata]).is_empty());
    assert_eq!(
        stream_loss_paths(pair, &[unknown_item]),
        vec!["deltas[0].unknown"]
    );
}

#[test]
fn open_responses_reasoning_signatures_are_canonical() {
    let pair = ProtocolTransform::global()
        .bind(
            OPEN_RESPONSES_2026_04_24,
            GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA,
        )
        .expect("registered protocol pair");

    assert!(
        response_loss_paths(
            pair,
            &AiResponse {
                items: vec![AiItem::thinking("", Some("opaque".into()))],
                ..AiResponse::new("response", "model")
            }
        )
        .is_empty()
    );
    assert!(
        stream_loss_paths(pair, &[AiStreamDelta::ThinkingSignature("opaque".into())]).is_empty()
    );
}
