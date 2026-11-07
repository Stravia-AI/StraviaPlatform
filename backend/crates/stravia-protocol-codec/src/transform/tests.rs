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

use super::{ProtocolTransform, ThinkingCarrierFacts, TransformError};
use stravia_runtime_contract::protocol::ids::ANTHROPIC_MESSAGES_2023_06_01;
use stravia_runtime_contract::protocol::ids::GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA;
use stravia_runtime_contract::protocol::ids::OPEN_RESPONSES_2026_04_24;
use stravia_runtime_contract::protocol::ids::OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1;
use stravia_runtime_contract::protocol::ir::AiItem;
use stravia_runtime_contract::protocol::ir::AiRequest;
use stravia_runtime_contract::protocol::ir::AiResponse;
use stravia_runtime_contract::protocol::ir::AiStreamDelta;
use stravia_runtime_contract::protocol::ir::ContentBlock;
use stravia_runtime_contract::protocol::ir::MediaSource;
use stravia_runtime_contract::protocol::ir::MessageContent;
use stravia_runtime_contract::protocol::ir::Role;
use stravia_runtime_contract::protocol::ir::ToolCall;

#[test]
fn gemini_tool_result_attachments_keep_payloads_and_call_boundaries() {
    let pair = ProtocolTransform::global()
        .bind(
            GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA,
            GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA,
        )
        .unwrap();
    let results = json!([
        {"id":"image-call","name":"read_file","response":{
            "output":"chart", "parts":[{"type":"text","text":"business data"}]
        },"parts":[
            {"inlineData":{"mimeType":"image/png","data":"aW1hZ2U="}},
            {"fileData":{"mimeType":"image/jpeg","fileUri":"https://example.test/chart.jpg"}}
        ]},
        {"id":"pdf-call","name":"read_file","response":{"output":"report","count":2},
         "parts":[{"inlineData":{"mimeType":"application/pdf","data":"JVBERi0xLjQ="}}]}
    ]);
    let request = pair.decode_request(json!({
        "contents":[
            {"role":"user","parts":[{"text":"Read both files."}]},
            {"role":"model","parts":[
                {"functionCall":{"id":"pdf-call","name":"read_file","args":{"Path":"report.pdf"}},"thoughtSignature":"c2lnbmF0dXJl"},
                {"functionCall":{"id":"image-call","name":"read_file","args":{"Path":"chart.png"}}}
            ]},
            {"role":"model","parts":[
                {"functionResponse":results[0]},
                {"functionResponse":results[1]}
            ]}
        ]
    })).unwrap();
    let restored: AiRequest =
        serde_json::from_value(serde_json::to_value(request).unwrap()).unwrap();
    let body = pair.encode_request(restored).unwrap().body;
    let parts = body["contents"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|content| content["parts"].as_array().unwrap());
    let actual: Vec<_> = parts
        .filter_map(|part| part.get("functionResponse"))
        .cloned()
        .collect();
    assert_eq!(actual.len(), 2);
    for (actual, expected) in actual.iter().zip(results.as_array().unwrap()) {
        for key in ["id", "name", "response"] {
            assert_eq!(actual[key], expected[key]);
        }
        let actual_parts = actual["parts"].as_array().unwrap();
        let expected_parts = expected["parts"].as_array().unwrap();
        assert_eq!(actual_parts.len(), expected_parts.len());
        for (actual, expected) in actual_parts.iter().zip(expected_parts) {
            if expected.get("inlineData").is_some() {
                assert_eq!(actual["inlineData"], expected["inlineData"]);
            } else {
                // URL 图片沿用现有 canonical 载体；验证资源身份，不要求可选 MIME 提示逐字回放。
                assert_eq!(
                    actual["fileData"]["fileUri"],
                    expected["fileData"]["fileUri"]
                );
            }
        }
    }
    let signed_call = body["contents"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|content| content["parts"].as_array().unwrap())
        .find(|part| part["functionCall"]["id"] == "pdf-call")
        .unwrap();
    assert_eq!(signed_call["thoughtSignature"], "c2lnbmF0dXJl");
}

#[test]
fn gemini_tool_result_media_converts_with_its_business_json() {
    for target in [ANTHROPIC_MESSAGES_2023_06_01, OPEN_RESPONSES_2026_04_24] {
        let pair = ProtocolTransform::global()
            .bind(GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA, target)
            .unwrap();
        let request = pair.decode_request(json!({
            "contents":[
                {"role":"model","parts":[{"functionCall":{"id":"read-1","name":"read_file","args":{}}}]},
                {"role":"user","parts":[{"functionResponse":{
                    "id":"read-1","name":"read_file","response":{"receipt":"receipt-42"},
                    "parts":[
                        {"inlineData":{"mimeType":"image/png","data":"aW1hZ2U="}},
                        {"fileData":{"mimeType":"application/pdf","fileUri":"https://example.test/report.pdf"}}
                    ]
                }}]}
            ]
        })).unwrap();
        let body = pair.encode_request(request).unwrap().body;
        if target == ANTHROPIC_MESSAGES_2023_06_01 {
            let result = body["messages"]
                .as_array()
                .unwrap()
                .iter()
                .flat_map(|message| message["content"].as_array().unwrap())
                .find(|block| block["type"] == "tool_result")
                .unwrap();
            assert_eq!(result["tool_use_id"], "read-1");
            assert_eq!(
                serde_json::from_str::<Value>(result["content"][0]["text"].as_str().unwrap())
                    .unwrap(),
                json!({"receipt":"receipt-42"})
            );
            assert_eq!(result["content"][1]["source"]["data"], "aW1hZ2U=");
            assert_eq!(
                result["content"][2]["source"]["url"],
                "https://example.test/report.pdf"
            );
        } else {
            let result = body["input"]
                .as_array()
                .unwrap()
                .iter()
                .find(|item| item["type"] == "function_call_output")
                .unwrap();
            assert_eq!(result["call_id"], "read-1");
            assert_eq!(
                serde_json::from_str::<Value>(result["output"][0]["text"].as_str().unwrap())
                    .unwrap(),
                json!({"receipt":"receipt-42"})
            );
            assert_eq!(
                result["output"][1]["image_url"],
                "data:image/png;base64,aW1hZ2U="
            );
            assert_eq!(
                result["output"][2]["file_url"],
                "https://example.test/report.pdf"
            );
        }
    }
}

#[test]
fn gemini_canonical_sampling_changes_override_retained_native_config() {
    let pair = ProtocolTransform::global()
        .bind(
            GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA,
            GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA,
        )
        .unwrap();
    let mut request = pair
        .decode_request(json!({
            "contents":[{"role":"user","parts":[{"text":"hello"}]}],
            "generationConfig":{
                "stopSequences":["old"], "seed":100,
                "frequencyPenalty":0.1, "presencePenalty":0.2, "topK":11
            }
        }))
        .unwrap();
    request.generation.stop = Some(vec!["new".into()]);
    request.generation.seed = Some(-7);
    request.generation.frequency_penalty = Some(0.6);
    request.generation.presence_penalty = Some(-0.4);
    let body = pair.encode_request(request).unwrap().body;
    assert_eq!(body["generationConfig"]["stopSequences"], json!(["new"]));
    assert_eq!(body["generationConfig"]["seed"], -7);
    assert_eq!(body["generationConfig"]["frequencyPenalty"], 0.6);
    assert_eq!(body["generationConfig"]["presencePenalty"], -0.4);
    assert_eq!(body["generationConfig"]["topK"].as_f64(), Some(11.0));
    assert_eq!(body["contents"][0]["parts"][0]["text"], "hello");
}

#[test]
fn document_conversion_keeps_files_and_supported_nested_content() {
    for target in [
        OPEN_RESPONSES_2026_04_24,
        OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
        GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA,
    ] {
        let pair = ProtocolTransform::global()
            .bind(ANTHROPIC_MESSAGES_2023_06_01, target)
            .unwrap();
        let request = pair.decode_request(json!({
            "model":"model", "max_tokens":100,
            "messages":[{"role":"user","content":[
                {"type":"document","source":{"type":"base64","media_type":"application/pdf","data":"JVBERg=="},"title":"report.pdf"},
                {"type":"document","source":{"type":"text","media_type":"text/plain","data":"plain document"}},
                {"type":"document","source":{"type":"content","content":[
                    {"type":"text","text":"nested text"},
                    {"type":"image","source":{"type":"base64","media_type":"image/png","data":"aGk="}}
                ]}}
            ]}]
        })).unwrap();
        let body = pair.encode_request(request).unwrap().body;
        let parts = if target == GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA {
            body["contents"][0]["parts"].as_array().unwrap()
        } else if target == OPEN_RESPONSES_2026_04_24 {
            body["input"][0]["content"].as_array().unwrap()
        } else {
            body["messages"][0]["content"].as_array().unwrap()
        };
        assert_eq!(parts.len(), 4, "{target}: {body}");
        assert!(
            parts.iter().any(|part| part["text"] == "nested text"),
            "{target}: {body}"
        );
        let wire = body.to_string();
        assert!(wire.contains("JVBERg=="), "{target}: {body}");
        assert!(wire.contains("cGxhaW4gZG9jdW1lbnQ="), "{target}: {body}");
        assert!(wire.contains("aGk="), "{target}: {body}");
    }
}

#[test]
fn native_anthropic_server_tool_blocks_survive_projection_and_encoding() {
    let pair = ProtocolTransform::global()
        .bind(ANTHROPIC_MESSAGES_2023_06_01, ANTHROPIC_MESSAGES_2023_06_01)
        .unwrap();
    let response = pair.decode_response(json!({
        "id":"msg_1", "type":"message", "role":"assistant", "model":"model",
        "content":[
            {"type":"server_tool_use","id":"srv_1","name":"web_search","input":{"query":"hello"}},
            {"type":"web_search_tool_result","tool_use_id":"srv_1","content":[{"type":"web_search_result","url":"https://example.com","title":"Example","encrypted_content":"opaque"}]}
        ],
        "stop_reason":"end_turn", "usage":{"input_tokens":1,"output_tokens":1}
    })).unwrap();
    let before = serde_json::to_value(&response).unwrap();
    assert!(matches!(
        super::prune_response(pair, &response),
        std::borrow::Cow::Borrowed(_)
    ));
    let encoded = pair.encode_response(&response).unwrap();
    assert_eq!(encoded["content"][0]["type"], "server_tool_use");
    assert_eq!(encoded["content"][1]["type"], "web_search_tool_result");
    assert_eq!(encoded["content"][1]["tool_use_id"], "srv_1");
    assert_eq!(serde_json::to_value(&response).unwrap(), before);
}

#[test]
fn tool_output_keeps_supported_blocks_distinct_from_business_json() {
    let pair = ProtocolTransform::global()
        .bind(ANTHROPIC_MESSAGES_2023_06_01, OPEN_RESPONSES_2026_04_24)
        .unwrap();
    let request = pair.decode_request(json!({
        "model":"model", "max_tokens":100,
        "messages":[
            {"role":"user","content":"inspect"},
            {"role":"assistant","content":[{"type":"tool_use","id":"call_1","name":"read","input":{}}]},
            {"role":"user","content":[{"type":"tool_result","tool_use_id":"call_1","content":[
                {"type":"text","text":"kept","future_hint":"omit"},
                {"type":"image","source":{"type":"base64","media_type":"image/png","data":"aGk="}},
                {"type":"future_part","payload":"cannot-carry"}
            ]}]}
        ]
    })).unwrap();
    let encoded = pair.encode_request(request).unwrap().body;
    let output = encoded["input"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["type"] == "function_call_output")
        .unwrap();
    assert_eq!(output["call_id"], "call_1");
    assert_eq!(
        output["output"],
        json!([
            {"type":"input_text","text":"kept"},
            {"type":"input_image","image_url":"data:image/png;base64,aGk="}
        ])
    );

    let business = json!([{"type":"input_image","image_url":"this is business data"}]);
    let request = AiRequest::new(
        "model",
        vec![AiItem {
            role: Role::Tool,
            content: MessageContent::Blocks(vec![ContentBlock::ToolResult {
                tool_use_id: "business_call".into(),
                content: business.clone(),
                content_kind: Some(
                    stravia_runtime_contract::protocol::ir::ToolResultContentKind::Json,
                ),
                is_error: None,
                cache_control: None,
            }]),
            tool_calls: None,
            tool_call_id: Some("business_call".into()),
            meta: None,
        }],
    );
    let encoded = ProtocolTransform::global()
        .bind(
            GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA,
            OPEN_RESPONSES_2026_04_24,
        )
        .unwrap()
        .encode_request(request)
        .unwrap()
        .body;
    assert_eq!(
        serde_json::from_str::<Value>(encoded["input"][0]["output"].as_str().unwrap()).unwrap(),
        business
    );
}

#[test]
fn historical_reasoning_replay_is_target_local_and_keeps_readable_parts() {
    let native = ProtocolTransform::global()
        .bind(OPEN_RESPONSES_2026_04_24, OPEN_RESPONSES_2026_04_24)
        .unwrap();
    let original = native.decode_request(json!({
        "model":"model", "input":[
            {"type":"reasoning", "id":"rs_1", "summary":[{"type":"summary_text","text":"summary"}],
             "content":[{"type":"reasoning_text","text":"detail"}], "encrypted_content":"secret", "native_hint":"opaque"},
            {"role":"user","content":"continue"}
        ]
    })).unwrap();
    let before = serde_json::to_value(&original).unwrap();
    let mut compatible = original.clone();
    assert!(!super::prepare_thinking_replay(&mut compatible, |_, _| {
        true
    }));
    assert_eq!(
        native.encode_request(compatible).unwrap().body["input"][0]["encrypted_content"],
        "secret"
    );

    for target in [
        OPEN_RESPONSES_2026_04_24,
        OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
        ANTHROPIC_MESSAGES_2023_06_01,
        GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA,
    ] {
        let mut replay = original.clone();
        assert!(super::prepare_thinking_replay(&mut replay, |_, _| false));
        let pair = ProtocolTransform::global()
            .bind(OPEN_RESPONSES_2026_04_24, target)
            .unwrap();
        let body = pair.encode_request(replay).unwrap().body;
        let serialized = body.to_string();
        assert!(serialized.contains("summary"));
        assert!(serialized.contains("detail"));
        assert!(serialized.contains("continue"));
        assert!(!serialized.contains("secret"));
        assert!(!serialized.contains("native_hint"));
        if target == OPEN_RESPONSES_2026_04_24 {
            assert_eq!(body["input"][0]["type"], "message");
            assert_eq!(body["input"][0]["content"][0]["text"], "summary");
            assert_eq!(body["input"][0]["content"][1]["text"], "detail");
        }
    }
    assert_eq!(serde_json::to_value(&original).unwrap(), before);
}

#[test]
fn thinking_replay_gemini_tool_signature_stays_on_function_call_part() {
    let pair = ProtocolTransform::global()
        .bind(
            GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA,
            GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA,
        )
        .unwrap();
    let native = pair.decode_request(json!({"contents":[
        {"role":"model","parts":[{
            "functionCall":{"id":"call_lookup","name":"lookup","args":{"key":"value"}},
            "thoughtSignature":"tool-signature"
        }]},
        {"role":"user","parts":[{
            "functionResponse":{"id":"call_lookup","name":"lookup","response":{"result":"found"}}
        }]}
    ]})).unwrap();
    let mut adjacent = native.clone();
    adjacent.items[0].content = MessageContent::Blocks(vec![ContentBlock::ToolUse {
        id: "call_lookup".into(),
        name: "lookup".into(),
        input: json!({"key":"value"}),
        cache_control: None,
    }]);
    adjacent
        .items
        .insert(0, AiItem::thinking("", Some("tool-signature".into())));
    for original in [native, adjacent] {
        for preserve in [true, false] {
            let mut replay = original.clone();
            super::prepare_thinking_replay(&mut replay, |_, _| preserve);
            let body = pair.encode_request(replay).unwrap().body;
            assert_eq!(body["contents"][0]["role"], "model");
            assert_eq!(
                body["contents"][0]["parts"],
                if preserve {
                    json!([{
                        "functionCall":{"id":"call_lookup","name":"lookup","args":{"key":"value"}},
                        "thoughtSignature":"tool-signature"
                    }])
                } else {
                    json!([{
                        "functionCall":{"id":"call_lookup","name":"lookup","args":{"key":"value"}}
                    }])
                }
            );
            assert_eq!(
                body["contents"][1]["parts"][0]["functionResponse"]["name"],
                "lookup"
            );
            assert_eq!(
                body["contents"][1]["parts"][0]["functionResponse"]["id"],
                "call_lookup"
            );
        }
    }
}

#[test]
fn thinking_replay_chat_response_tools_survive_gemini_history_encoding() {
    let chat = ProtocolTransform::global()
        .bind(
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
        )
        .unwrap();
    let response = chat
        .decode_response(json!({
            "id":"chat_response", "model":"chat-model", "choices":[{
                "index":0, "finish_reason":"tool_calls", "message":{
                    "role":"assistant", "content":"I will inspect the repository.",
                    "reasoning_content":"Inspect the files first.",
                    "tool_calls":[{"id":"call_lookup","type":"function","function":{
                        "name":"lookup","arguments":"{\"key\":\"value\"}"
                    }}]
                }
            }]
        }))
        .unwrap();
    let history = crate::codec::openai::compatible::stream::client_history_output_item(&response);
    let mut dual = history.clone();
    let MessageContent::Blocks(blocks) = &mut dual.content else {
        panic!("readable history blocks")
    };
    blocks.push(ContentBlock::ToolUse {
        id: "call_lookup".into(),
        name: "lookup".into(),
        input: json!({"key":"value"}),
        cache_control: None,
    });
    let gemini = ProtocolTransform::global()
        .bind(
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA,
        )
        .unwrap();
    for mut items in [response.items, vec![history], vec![dual]] {
        items.push(AiItem::function_call_output("call_lookup", json!("found")));
        let body = gemini
            .encode_request(AiRequest::new("model", items))
            .unwrap()
            .body;
        let parts: Vec<_> = body["contents"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|content| content["role"] == "model")
            .flat_map(|content| content["parts"].as_array().unwrap().iter())
            .collect();
        let readable: Vec<_> = parts
            .iter()
            .filter(|part| part.get("text").is_some())
            .map(|part| (*part).clone())
            .collect();
        assert_eq!(
            readable,
            vec![
                json!({"text":"Inspect the files first.","thought":true}),
                json!({"text":"I will inspect the repository."})
            ]
        );
        let calls: Vec<_> = parts
            .iter()
            .filter_map(|part| part.get("functionCall"))
            .cloned()
            .collect();
        assert_eq!(
            calls,
            vec![json!({"id":"call_lookup","name":"lookup","args":{"key":"value"}})]
        );
        let contents = body["contents"].as_array().unwrap();
        let result = &contents.last().unwrap()["parts"][0]["functionResponse"];
        assert_eq!(result["name"], "lookup");
        assert_eq!(result["response"], json!({"result":"found"}));
    }
}

#[test]
fn thinking_replay_matrix_keeps_mixed_text_and_tool_associations() {
    let original = AiRequest::new(
        "model",
        vec![
            AiItem {
                role: Role::Assistant,
                content: MessageContent::Blocks(vec![
                    ContentBlock::Text {
                        text: "before".to_owned().into(),
                        cache_control: None,
                    },
                    ContentBlock::Thinking {
                        thinking: "inspect".into(),
                        signature: Some("signed".into()),
                    },
                    ContentBlock::Text {
                        text: "after".to_owned().into(),
                        cache_control: None,
                    },
                ]),
                tool_calls: Some(vec![ToolCall {
                    id: "call_lookup".into(),
                    name: "lookup".into(),
                    arguments: "{}".into(),
                }]),
                tool_call_id: None,
                meta: None,
            },
            AiItem::function_call_output("call_lookup", json!("result")),
        ],
    );
    for target in [
        OPEN_RESPONSES_2026_04_24,
        ANTHROPIC_MESSAGES_2023_06_01,
        GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA,
        OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
    ] {
        let pair = ProtocolTransform::global()
            .bind(ANTHROPIC_MESSAGES_2023_06_01, target)
            .unwrap();
        for preserve in [false, true] {
            let mut replay = original.clone();
            assert_eq!(
                super::prepare_thinking_replay(&mut replay, |_, _| preserve),
                !preserve
            );
            let body = pair.encode_request(replay).unwrap().body;
            match target {
                OPEN_RESPONSES_2026_04_24 => {
                    assert_eq!(
                        body["input"][0]["content"],
                        json!([
                            {"type":"output_text","text":"before"},
                            {"type":"output_text","text":"inspect"},
                            {"type":"output_text","text":"after"}
                        ])
                    );
                    assert_eq!(body["input"][1]["type"], "function_call");
                    assert_eq!(body["input"][1]["call_id"], "call_lookup");
                    assert_eq!(body["input"][2]["type"], "function_call_output");
                    assert_eq!(body["input"][2]["call_id"], "call_lookup");
                    assert_eq!(body["input"][2]["output"], "result");
                    assert!(!body.to_string().contains("signed"));
                }
                ANTHROPIC_MESSAGES_2023_06_01 => {
                    let content = &body["messages"][0]["content"];
                    if preserve {
                        assert_eq!(
                            content[0],
                            json!({"type":"thinking","thinking":"inspect","signature":"signed"})
                        );
                        assert_eq!(content[1], json!({"type":"text","text":"before"}));
                    } else {
                        assert_eq!(content[0], json!({"type":"text","text":"before"}));
                        assert_eq!(content[1], json!({"type":"text","text":"inspect"}));
                    }
                    assert_eq!(content[2], json!({"type":"text","text":"after"}));
                    assert_eq!(content[3]["type"], "tool_use");
                    assert_eq!(content[3]["id"], "call_lookup");
                    assert_eq!(
                        body["messages"][1]["content"][0]["tool_use_id"],
                        "call_lookup"
                    );
                    assert_eq!(body["messages"][1]["content"][0]["content"], "result");
                }
                GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA => {
                    let parts = body["contents"][0]["parts"]
                        .as_array()
                        .expect("model parts");
                    let readable: Vec<_> = parts
                        .iter()
                        .filter(|part| part.get("text").is_some())
                        .cloned()
                        .collect();
                    assert_eq!(
                        readable,
                        vec![
                            json!({"text":"before"}),
                            if preserve {
                                json!({"text":"inspect","thought":true,"thoughtSignature":"signed"})
                            } else {
                                json!({"text":"inspect","thought":true})
                            },
                            json!({"text":"after"})
                        ]
                    );
                    let calls: Vec<_> = parts
                        .iter()
                        .filter_map(|part| part.get("functionCall"))
                        .collect();
                    assert_eq!(
                        calls,
                        vec![&json!({"id":"call_lookup","name":"lookup","args":{}})]
                    );
                    let result = &body["contents"][1]["parts"][0]["functionResponse"];
                    assert_eq!(result["name"], "lookup");
                    assert_eq!(result["response"], json!({"result":"result"}));
                    assert!(!body.to_string().contains("signed") || preserve);
                }
                OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1 => {
                    assert_eq!(body["messages"][0]["reasoning_content"], "inspect");
                    assert_eq!(
                        body["messages"][0]["content"],
                        json!([
                            {"type":"text","text":"before"}, {"type":"text","text":"after"}
                        ])
                    );
                    assert_eq!(body["messages"][0]["tool_calls"][0]["id"], "call_lookup");
                    assert_eq!(body["messages"][1]["tool_call_id"], "call_lookup");
                    assert_eq!(body["messages"][1]["content"], "result");
                    assert!(!body.to_string().contains("signed"));
                }
                _ => unreachable!(),
            }
        }
    }
}

#[test]
fn wire_request_matrix_preserves_readable_reasoning_and_tool_continuations() {
    let sources = [
        (
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            json!({"model":"model","messages":[
                {"role":"assistant","content":"answer","reasoning_content":"inspect",
                 "tool_calls":[{"id":"call_lookup","type":"function","function":{
                     "name":"lookup","arguments":"{\"key\":\"value\"}"}}]},
                {"role":"tool","tool_call_id":"call_lookup","content":"found"}
            ]}),
        ),
        (
            ANTHROPIC_MESSAGES_2023_06_01,
            json!({"model":"model","max_tokens":100,"messages":[
                {"role":"assistant","content":[
                    {"type":"thinking","thinking":"inspect","signature":"native-signature"},
                    {"type":"text","text":"answer"},
                    {"type":"tool_use","id":"call_lookup","name":"lookup","input":{"key":"value"}}
                ]},
                {"role":"user","content":[
                    {"type":"tool_result","tool_use_id":"call_lookup","content":"found"}
                ]}
            ]}),
        ),
        (
            GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA,
            json!({"model":"model","contents":[
                {"role":"model","parts":[
                    {"text":"inspect","thought":true},
                    {"text":"answer"},
                    {"functionCall":{"id":"call_lookup","name":"lookup","args":{"key":"value"}}}
                ]},
                {"role":"user","parts":[
                    {"functionResponse":{"id":"call_lookup","name":"lookup","response":{"result":"found"}}}
                ]}
            ]}),
        ),
        (
            OPEN_RESPONSES_2026_04_24,
            json!({"model":"model","input":[
                {"type":"reasoning","summary":[{"type":"summary_text","text":"inspect"}]},
                {"role":"assistant","content":[{"type":"output_text","text":"answer"}]},
                {"type":"function_call","call_id":"call_lookup","name":"lookup","arguments":"{\"key\":\"value\"}"},
                {"type":"function_call_output","call_id":"call_lookup","output":"found"}
            ]}),
        ),
    ];
    for (source, wire) in sources {
        for target in [
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            ANTHROPIC_MESSAGES_2023_06_01,
            GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA,
            OPEN_RESPONSES_2026_04_24,
        ] {
            let pair = ProtocolTransform::global().bind(source, target).unwrap();
            let request = pair.decode_request(wire.clone()).unwrap();
            let body = pair.encode_request(request).unwrap().body;
            match target {
                OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1 => {
                    let messages = body["messages"].as_array().unwrap();
                    let reasoning: Vec<_> = messages
                        .iter()
                        .filter_map(|message| message["reasoning_content"].as_str())
                        .collect();
                    assert_eq!(reasoning, ["inspect"]);
                    let answers: Vec<_> = messages
                        .iter()
                        .filter(|message| message["role"] == "assistant")
                        .flat_map(|message| {
                            message["content"].as_str().into_iter().chain(
                                message["content"]
                                    .as_array()
                                    .into_iter()
                                    .flatten()
                                    .filter_map(|part| part["text"].as_str()),
                            )
                        })
                        .filter(|text| !text.is_empty())
                        .collect();
                    assert_eq!(answers, ["answer"]);
                    let calls: Vec<_> = messages
                        .iter()
                        .flat_map(|message| message["tool_calls"].as_array().into_iter().flatten())
                        .collect();
                    assert_eq!(calls.len(), 1);
                    let call = calls[0];
                    assert_eq!(call["id"], "call_lookup");
                    assert_eq!(call["function"]["name"], "lookup");
                    assert_eq!(
                        serde_json::from_str::<Value>(
                            call["function"]["arguments"].as_str().unwrap()
                        )
                        .unwrap(),
                        json!({"key":"value"})
                    );
                    let result = messages
                        .iter()
                        .find(|message| message["role"] == "tool")
                        .unwrap();
                    assert_eq!(result["tool_call_id"], "call_lookup");
                    if source == GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA {
                        assert_eq!(
                            serde_json::from_str::<Value>(result["content"].as_str().unwrap())
                                .unwrap(),
                            json!({"result":"found"})
                        );
                    } else {
                        assert_eq!(result["content"], "found");
                    }
                }
                ANTHROPIC_MESSAGES_2023_06_01 => {
                    let messages = body["messages"].as_array().unwrap();
                    let blocks: Vec<_> = messages
                        .iter()
                        .filter(|message| message["role"] == "assistant")
                        .flat_map(|message| message["content"].as_array().unwrap())
                        .collect();
                    if source == ANTHROPIC_MESSAGES_2023_06_01 {
                        assert_eq!(
                            blocks[0],
                            &json!({
                                "type":"thinking","thinking":"inspect","signature":"native-signature"
                            })
                        );
                    } else {
                        // Unsigned/foreign readable reasoning is replayable text, not a forged signature.
                        assert_eq!(blocks[0], &json!({"type":"text","text":"inspect"}));
                    }
                    assert_eq!(blocks[1], &json!({"type":"text","text":"answer"}));
                    assert_eq!(
                        blocks[2],
                        &json!({
                            "type":"tool_use","id":"call_lookup","name":"lookup","input":{"key":"value"}
                        })
                    );
                    let result = messages
                        .iter()
                        .flat_map(|message| message["content"].as_array().unwrap())
                        .find(|block| block["type"] == "tool_result")
                        .unwrap();
                    assert_eq!(result["type"], "tool_result");
                    assert_eq!(result["tool_use_id"], "call_lookup");
                    if source == GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA {
                        assert_eq!(
                            serde_json::from_str::<Value>(result["content"].as_str().unwrap())
                                .unwrap(),
                            json!({"result":"found"})
                        );
                    } else {
                        assert_eq!(result["content"], "found");
                    }
                }
                GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA => {
                    let contents = body["contents"].as_array().unwrap();
                    let parts: Vec<_> = contents
                        .iter()
                        .filter(|content| content["role"] == "model")
                        .flat_map(|content| content["parts"].as_array().unwrap())
                        .collect();
                    let readable: Vec<_> = parts
                        .iter()
                        .filter_map(|part| {
                            part["text"]
                                .as_str()
                                .map(|text| (text, part["thought"] == true))
                        })
                        .collect();
                    assert_eq!(readable, [("inspect", true), ("answer", false)]);
                    let calls: Vec<_> = parts
                        .iter()
                        .filter_map(|part| part.get("functionCall"))
                        .collect();
                    assert_eq!(
                        calls,
                        [&json!({"id":"call_lookup","name":"lookup","args":{"key":"value"}})]
                    );
                    assert_eq!(
                        contents.last().unwrap()["parts"][0]["functionResponse"],
                        json!({
                            "id":"call_lookup","name":"lookup","response":{"result":"found"}
                        })
                    );
                }
                OPEN_RESPONSES_2026_04_24 => {
                    let input = body["input"].as_array().unwrap();
                    let readable: Vec<_> = input
                        .iter()
                        .flat_map(|item| {
                            let parts = if item["type"] == "reasoning" {
                                item["summary"].as_array()
                            } else {
                                item["content"].as_array()
                            };
                            parts
                                .into_iter()
                                .flatten()
                                .filter_map(|part| part["text"].as_str())
                        })
                        .collect();
                    assert_eq!(readable, ["inspect", "answer"]);
                    let call = input
                        .iter()
                        .find(|item| item["type"] == "function_call")
                        .unwrap();
                    assert_eq!(call["call_id"], "call_lookup");
                    assert_eq!(call["name"], "lookup");
                    assert_eq!(
                        serde_json::from_str::<Value>(call["arguments"].as_str().unwrap()).unwrap(),
                        json!({"key":"value"})
                    );
                    let result = input
                        .iter()
                        .find(|item| item["type"] == "function_call_output")
                        .unwrap();
                    assert_eq!(result["call_id"], "call_lookup");
                    if source == GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA {
                        assert_eq!(
                            serde_json::from_str::<Value>(result["output"].as_str().unwrap())
                                .unwrap(),
                            json!({"result":"found"})
                        );
                    } else {
                        assert_eq!(result["output"], "found");
                    }
                    assert!(
                        input
                            .iter()
                            .all(|item| item.get("encrypted_content").is_none())
                    );
                }
                _ => unreachable!(),
            }
        }
    }
}

#[test]
fn anthropic_tool_result_preserves_json_array_and_native_block_meanings() {
    use stravia_runtime_contract::protocol::ir::ToolResultContentKind;

    let payload = json!([{"type": "text", "text": "business value"}]);
    for kind in [
        ToolResultContentKind::Json,
        ToolResultContentKind::ContentBlocks,
    ] {
        let request = AiRequest::new(
            "model",
            vec![
                AiItem::function_call(ToolCall {
                    id: "call_lookup".into(),
                    name: "lookup".into(),
                    arguments: "{}".into(),
                }),
                AiItem {
                    role: Role::Tool,
                    content: MessageContent::Blocks(vec![ContentBlock::ToolResult {
                        tool_use_id: "call_lookup".into(),
                        content: payload.clone(),
                        content_kind: Some(kind),
                        is_error: None,
                        cache_control: None,
                    }]),
                    tool_calls: None,
                    tool_call_id: Some("call_lookup".into()),
                    meta: None,
                },
            ],
        );
        let pair = ProtocolTransform::global()
            .bind(
                OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
                ANTHROPIC_MESSAGES_2023_06_01,
            )
            .unwrap();
        let body = pair.encode_request(request).unwrap().body;
        let result = &body["messages"][1]["content"][0]["content"];
        if kind == ToolResultContentKind::Json {
            assert_eq!(
                serde_json::from_str::<Value>(result.as_str().unwrap()).unwrap(),
                payload
            );
        } else {
            assert_eq!(result, &payload);
        }
    }
}

#[test]
fn single_native_text_part_uses_the_common_chat_string_carrier() {
    let pair = ProtocolTransform::global()
        .bind(
            ANTHROPIC_MESSAGES_2023_06_01,
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
        )
        .unwrap();
    let request = pair.decode_request(json!({
        "model": "model",
        "max_tokens": 256,
        "messages": [
            {"role": "user", "content": "Reply READY."},
            {"role": "assistant", "content": [
                {"type": "thinking", "thinking": "Inspect the requested reply.", "signature": "native-signature"},
                {"type": "text", "text": "\nREADY"}
            ]},
            {"role": "user", "content": "Continue."}
        ]
    })).unwrap();
    let body = pair.encode_request(request).unwrap().body;
    assert_eq!(body["messages"][1]["content"].as_str(), Some("\nREADY"));
    assert_eq!(
        body["messages"][1]["reasoning_content"],
        "Inspect the requested reply."
    );
}

#[test]
fn explicit_chat_reasoning_presence_stays_native_across_response_roundtrips() {
    use stravia_runtime_contract::protocol::ir::vendor_ext::CHAT_REASONING_FIELD_META;

    let chat = ProtocolTransform::global()
        .bind(
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
        )
        .unwrap();
    for value in [Some(json!("")), Some(Value::Null), None] {
        let mut message = json!({"role":"assistant","content":"answer"});
        if let Some(value) = &value {
            message["reasoning"] = value.clone();
        }
        let response = chat
            .decode_response(json!({
                "id":"chat_presence","model":"model","choices":[{
                    "index":0,"finish_reason":"stop","message":message
                }]
            }))
            .unwrap();
        for client in [
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            ANTHROPIC_MESSAGES_2023_06_01,
            GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA,
            OPEN_RESPONSES_2026_04_24,
        ] {
            let outbound = ProtocolTransform::global()
                .bind(client, OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1)
                .unwrap();
            let body = outbound.encode_response(&response).unwrap();
            assert!(!body.to_string().contains(CHAT_REASONING_FIELD_META));
            match client {
                OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1 => {
                    assert_eq!(
                        body["choices"][0]["message"].get("reasoning_content"),
                        value.as_ref()
                    );
                    assert_eq!(body["choices"][0]["message"]["content"], "answer");
                }
                ANTHROPIC_MESSAGES_2023_06_01 => {
                    assert_eq!(body["content"], json!([{"type":"text","text":"answer"}]));
                }
                GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA => {
                    assert_eq!(
                        body["candidates"][0]["content"]["parts"],
                        json!([{"text":"answer"}])
                    );
                }
                OPEN_RESPONSES_2026_04_24 => {
                    let output = body["output"].as_array().unwrap();
                    assert_eq!(output.len(), 1);
                    assert_eq!(output[0]["type"], "message");
                    assert_eq!(output[0]["content"][0]["type"], "output_text");
                    assert_eq!(output[0]["content"][0]["text"], "answer");
                }
                _ => unreachable!(),
            }
            let inbound = ProtocolTransform::global()
                .bind(OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1, client)
                .unwrap();
            let replay = inbound.decode_response(body).unwrap();
            let replayed_chat = inbound.encode_response(&replay).unwrap();
            let message = &replayed_chat["choices"][0]["message"];
            assert_eq!(message["content"], "answer");
            if client == OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1 {
                assert_eq!(message.get("reasoning_content"), value.as_ref());
            } else {
                assert!(message.get("reasoning_content").is_none());
            }
        }
    }
}

#[test]
fn prune_reports_original_indices_across_both_removal_passes() {
    let duplicate = AiItem::output_text("same");
    let mut empty = AiItem::output_text("discarded");
    empty.content = MessageContent::Blocks(Vec::new());
    let mut request = AiRequest::new(
        "model",
        vec![
            duplicate.clone(),
            AiItem::unknown(json!({"type":"unknown"})),
            duplicate,
            empty,
        ],
    );
    request.meta.source_protocol = Some(OPEN_RESPONSES_2026_04_24);
    let capabilities = super::adapter(ANTHROPIC_MESSAGES_2023_06_01)
        .unwrap()
        .capabilities();
    let removed = ProtocolTransform::prune_request_for_with_removals(
        ANTHROPIC_MESSAGES_2023_06_01,
        capabilities,
        &mut request,
    );
    assert_eq!(removed, vec![1, 3]);
    assert_eq!(request.items.len(), 2);
}

#[test]
fn replay_drops_only_empty_thinking_and_retains_tools_and_hard_fields() {
    let pair = ProtocolTransform::global()
        .bind(
            OPEN_RESPONSES_2026_04_24,
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
        )
        .unwrap();
    let mut request = pair
        .decode_request(json!({"model":"model","input":[
            {"type":"reasoning","summary":[],"encrypted_content":"secret"},
            {"role":"user","content":"continue"}
        ]}))
        .unwrap();
    let mut mixed = request.items[0].clone();
    mixed.tool_calls = Some(vec![ToolCall {
        id: "call_1".into(),
        name: "lookup".into(),
        arguments: "{}".into(),
    }]);
    mixed.meta = Some(
        stravia_runtime_contract::protocol::ir::AiItemMetadata::boxed(
            json!({"ordinary":"retained"}),
        ),
    );
    request.items.insert(1, mixed);
    request.items.insert(
        2,
        AiItem::function_call_output("call_1", json!("lookup result")),
    );
    assert!(super::prepare_thinking_replay(&mut request, |_, _| false));
    assert_eq!(request.items.len(), 3);
    assert_eq!(
        request.items[0]
            .meta
            .as_ref()
            .unwrap()
            .get("ordinary")
            .unwrap(),
        "retained"
    );
    let body = pair.encode_request(request.clone()).unwrap().body;
    assert_eq!(body["messages"][0]["tool_calls"][0]["id"], "call_1");
    assert_eq!(body["messages"][1]["tool_call_id"], "call_1");
    assert_eq!(body["messages"][1]["content"], "lookup result");
    assert_eq!(body["messages"][2]["content"], "continue");

    let MessageContent::Blocks(blocks) = &mut request.items[0].content else {
        panic!("blocks")
    };
    blocks.push(ContentBlock::Compaction {
        encrypted_content: "compaction-secret".into(),
        summary: None,
    });
    assert!(!super::prepare_thinking_replay(&mut request, |_, _| false));
    let body = pair.encode_request(request).unwrap().body;
    assert!(
        !body.to_string().contains("compaction-secret"),
        "native compaction state the target cannot carry is dropped, not forwarded"
    );
    assert_eq!(body["messages"][0]["tool_calls"][0]["id"], "call_1");
}

#[test]
fn replay_retains_native_signed_thinking_but_not_foreign_signatures_or_redactions() {
    let native = ProtocolTransform::global()
        .bind(ANTHROPIC_MESSAGES_2023_06_01, ANTHROPIC_MESSAGES_2023_06_01)
        .unwrap();
    let original = native.decode_request(json!({"model":"model","max_tokens":100,"messages":[
        {"role":"assistant","content":[{"type":"thinking","thinking":"visible","signature":"signed"},{"type":"redacted_thinking","data":"hidden"},{"type":"text","text":"answer"}]},
        {"role":"user","content":"continue"}
    ]})).unwrap();
    let mut same = original.clone();
    assert!(!super::prepare_thinking_replay(&mut same, |_, _| true));
    let encoded = native.encode_request(same).unwrap().body;
    assert_eq!(
        encoded["messages"][0]["content"],
        json!([
            {"type":"thinking","thinking":"visible","signature":"signed"},
            {"type":"redacted_thinking","data":"hidden"},
            {"type":"text","text":"answer"}
        ])
    );
    let mut foreign = original.clone();
    assert!(super::prepare_thinking_replay(&mut foreign, |_, _| false));
    let body = native.encode_request(foreign).unwrap().body;
    assert_eq!(
        body["messages"][0]["content"][0],
        json!({"type":"text","text":"visible"})
    );
    assert_eq!(
        body["messages"][0]["content"][1],
        json!({"type":"text","text":"answer"})
    );

    let chat = ProtocolTransform::global()
        .bind(
            ANTHROPIC_MESSAGES_2023_06_01,
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
        )
        .unwrap();
    // 来源可证明时保留签名；Chat 承载不了签名与 redacted，由 codec 忽略，
    // 明文仍走原生 reasoning_content，不混入正文。
    let mut replay = original;
    assert!(!super::prepare_thinking_replay(&mut replay, |_, _| true));
    let body = chat.encode_request(replay.clone()).unwrap().body;
    assert_eq!(body["messages"][0]["reasoning_content"], "visible");
    assert!(
        !body["messages"][0]["content"]
            .to_string()
            .contains("visible")
    );
    assert!(!body.to_string().contains("signed"));
    assert!(!body.to_string().contains("hidden"));

    let responses = ProtocolTransform::global()
        .bind(ANTHROPIC_MESSAGES_2023_06_01, OPEN_RESPONSES_2026_04_24)
        .unwrap();
    let body = responses.encode_request(replay).unwrap().body;
    assert_eq!(body["input"][0]["type"], "message");
    assert_eq!(
        body["input"][0]["content"],
        json!([
            {"type":"output_text","text":"visible"},
            {"type":"output_text","text":"answer"}
        ])
    );
    assert!(
        body["input"]
            .as_array()
            .unwrap()
            .iter()
            .all(|item| item.get("encrypted_content").is_none())
    );
    assert!(!body.to_string().contains("signed"));
    assert!(!body.to_string().contains("hidden"));
}

#[test]
fn replay_selects_each_protected_payload_and_keeps_native_fields_until_all_are_stripped() {
    let native = ProtocolTransform::global()
        .bind(OPEN_RESPONSES_2026_04_24, OPEN_RESPONSES_2026_04_24)
        .unwrap();
    let mut original = native.decode_request(json!({"model":"model","input":[
        {"type":"reasoning","id":"rs_local","native_hint":"retained","summary":[{"type":"summary_text","text":"local summary"}],"encrypted_content":"local encrypted"},
        {"role":"user","content":"continue"}
    ]})).unwrap();
    let MessageContent::Blocks(blocks) = &mut original.items[0].content else {
        panic!("blocks")
    };
    blocks.extend([
        ContentBlock::Reasoning {
            summary: vec!["foreign summary".into()],
            content: vec!["foreign detail".into()],
            encrypted_content: Some("foreign encrypted".into()),
        },
        ContentBlock::Thinking {
            thinking: "local thought".into(),
            signature: Some("local signature".into()),
        },
        ContentBlock::Thinking {
            thinking: "foreign thought".into(),
            signature: Some("foreign signature".into()),
        },
        ContentBlock::RedactedThinking {
            data: "local redacted".into(),
        },
        ContentBlock::RedactedThinking {
            data: "foreign redacted".into(),
        },
        // 未携带受保护载荷的空块不应因相邻载荷被剥离而消失。
        ContentBlock::Thinking {
            thinking: String::new(),
            signature: None,
        },
    ]);
    let before = serde_json::to_value(&original).unwrap();
    let mut replay = original.clone();
    assert!(super::prepare_thinking_replay(
        &mut replay,
        |item, block| {
            assert_eq!(
                item.meta
                    .as_ref()
                    .unwrap()
                    .get("__open_responses_item_fields"),
                original.items[0]
                    .meta
                    .as_ref()
                    .unwrap()
                    .get("__open_responses_item_fields")
            );
            match block {
                ContentBlock::Thinking {
                    signature: Some(value),
                    ..
                }
                | ContentBlock::Reasoning {
                    encrypted_content: Some(value),
                    ..
                }
                | ContentBlock::RedactedThinking { data: value } => value.starts_with("local"),
                _ => panic!("only protected payloads require an origin decision"),
            }
        }
    ));
    let MessageContent::Blocks(blocks) = &replay.items[0].content else {
        panic!("blocks")
    };
    assert_eq!(
        serde_json::to_value(blocks).unwrap(),
        serde_json::to_value(vec![
            ContentBlock::Reasoning {
                summary: vec!["local summary".into()],
                content: vec![],
                encrypted_content: Some("local encrypted".into()),
            },
            ContentBlock::Reasoning {
                summary: vec!["foreign summary".into()],
                content: vec!["foreign detail".into()],
                encrypted_content: None,
            },
            ContentBlock::Thinking {
                thinking: "local thought".into(),
                signature: Some("local signature".into()),
            },
            ContentBlock::Thinking {
                thinking: "foreign thought".into(),
                signature: None,
            },
            ContentBlock::RedactedThinking {
                data: "local redacted".into(),
            },
            ContentBlock::Thinking {
                thinking: String::new(),
                signature: None,
            },
        ])
        .unwrap()
    );
    assert_eq!(replay.items[0].meta, original.items[0].meta);
    let stable = serde_json::to_value(&replay).unwrap();
    assert!(!super::prepare_thinking_replay(&mut replay, |_, _| true));
    assert_eq!(serde_json::to_value(&replay).unwrap(), stable);

    // 最后一批本上游载荷被显式剥离后，才允许移除该思考条目的原生字段。
    assert!(super::prepare_thinking_replay(&mut replay, |_, _| false));
    assert!(
        replay.items[0]
            .meta
            .as_ref()
            .unwrap()
            .get("__open_responses_item_fields")
            .is_none()
    );
    let body = native.encode_request(replay).unwrap().body.to_string();
    for text in [
        "local summary",
        "foreign summary",
        "foreign detail",
        "local thought",
        "foreign thought",
    ] {
        assert!(body.contains(text));
    }
    for stripped in [
        "local encrypted",
        "foreign encrypted",
        "local signature",
        "foreign signature",
        "local redacted",
        "foreign redacted",
        "rs_local",
        "native_hint",
    ] {
        assert!(!body.contains(stripped));
    }
    assert_eq!(serde_json::to_value(&original).unwrap(), before);
}

#[test]
fn thinking_replay_native_reasoning_keeps_ciphertext_but_moves_content_before_tools() {
    let native = ProtocolTransform::global()
        .bind(OPEN_RESPONSES_2026_04_24, OPEN_RESPONSES_2026_04_24)
        .unwrap();
    let mut request = native.decode_request(json!({"model":"model","input":[
        {"type":"reasoning","id":"rs_native","native_hint":"reasoning-only","summary":[{"type":"summary_text","text":"summary-one"},{"type":"summary_text","text":"summary-two"}],"content":[{"type":"reasoning_text","text":"detail-one"},{"type":"reasoning_text","text":"detail-two"}],"encrypted_content":"secret"},
        {"role":"user","content":"continue"}
    ]})).unwrap();
    request.items[0].tool_calls = Some(vec![ToolCall {
        id: "call_1".into(),
        name: "lookup".into(),
        arguments: "{}".into(),
    }]);
    // 可证明来源的混合条目原样交给 codec；Responses encoder 自行拆出独立 reasoning item。
    assert!(!super::prepare_thinking_replay(&mut request, |_, _| true));
    let body = native.encode_request(request.clone()).unwrap().body;
    assert_eq!(body["input"][0]["encrypted_content"], "secret");
    assert_eq!(body["input"][0]["native_hint"], "reasoning-only");
    assert_eq!(body["input"][0]["content"], json!([]));
    assert_eq!(
        body["input"][0]["summary"],
        json!([
            { "type": "summary_text", "text": "summary-one" },
            { "type": "summary_text", "text": "summary-two" }
        ])
    );
    assert_eq!(body["input"][1]["type"], "message");
    assert_eq!(body["input"][1]["role"], "assistant");
    assert!(body["input"][1].get("id").is_none());
    assert!(body["input"][1].get("native_hint").is_none());
    assert_eq!(
        body["input"][1]["content"],
        json!([
            { "type": "output_text", "text": "detail-one" },
            { "type": "output_text", "text": "detail-two" }
        ])
    );
    assert_eq!(body["input"][2]["type"], "function_call");
    assert_eq!(body["input"][2]["call_id"], "call_1");
}

#[test]
fn provider_message_metadata_does_not_interrupt_chat_delivery() {
    let pair = ProtocolTransform::global()
        .bind(
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            OPEN_RESPONSES_2026_04_24,
        )
        .unwrap();
    let item = json!({
        "type": "message", "id": "msg_1", "status": "completed",
        "role": "assistant", "phase": "commentary",
        "content": [{"type": "output_text", "text": "Checking hardware.",
                     "annotations": [], "logprobs": []}],
        "internal_chat_message_metadata_passthrough": {
            "create_time": 1789103215.391322, "turn_id": "turn_1"
        },
        "metadata": {"turn_id": "turn_1"},
        "provider_extension": {"future_field": true}
    });
    let (mut decoder, mut encoder) = pair.stream().unwrap().into_parts();
    let mut delivered = Vec::new();
    for event in [
        json!({"type":"response.created","response":
            dated_response("resp_1", "in_progress", json!([]), Value::Null)
        }),
        json!({"type":"response.output_item.added","output_index":0,"item":{
            "type":"message","id":"msg_1","role":"assistant",
            "status":"in_progress","content":[]
        }}),
        json!({"type":"response.output_item.done","output_index":0,"item":item}),
        json!({"type":"response.output_item.added","output_index":1,"item":{
            "type":"function_call","id":"fc_1","call_id":"call_1",
            "name":"Bash","arguments":"","status":"in_progress"
        }}),
        json!({"type":"response.function_call_arguments.delta","output_index":1,
            "item_id":"fc_1","delta":"{\"command\":\"hostname\"}"}),
    ] {
        let wire = format!(
            "event: {}\ndata: {event}\n\n",
            event["type"].as_str().unwrap()
        );
        let deltas = decoder.decode_chunk(wire.as_bytes()).unwrap();
        delivered.extend(
            encoder
                .encode_deltas(&deltas)
                .expect("deliver ordinary message and subsequent tool"),
        );
    }
    let chunks: Vec<Value> = delivered
        .iter()
        .filter_map(|event| serde_json::from_str(&event.data).ok())
        .collect();
    assert!(
        chunks
            .iter()
            .any(|chunk| chunk["choices"][0]["delta"]["content"] == "Checking hardware.")
    );
    assert!(
        chunks.iter().any(
            |chunk| chunk["choices"][0]["delta"]["tool_calls"][0]["function"]["name"] == "Bash"
        )
    );
    assert!(chunks.iter().any(
        |chunk| chunk["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"]
            == "{\"command\":\"hostname\"}"
    ));
    let response = pair
        .decode_response(dated_response(
            "resp_1",
            "completed",
            json!([item]),
            Value::Null,
        ))
        .unwrap();
    assert_eq!(
        pair.encode_response(&response).unwrap()["choices"][0]["message"]["content"],
        "Checking hardware."
    );
    let native = ProtocolTransform::global()
        .bind(OPEN_RESPONSES_2026_04_24, OPEN_RESPONSES_2026_04_24)
        .unwrap();
    let output = native.encode_response(&response).unwrap();
    for field in [
        "metadata",
        "internal_chat_message_metadata_passthrough",
        "provider_extension",
    ] {
        assert_eq!(output["output"][0][field], item[field]);
    }
    let outbound = ProtocolTransform::global()
        .bind(
            OPEN_RESPONSES_2026_04_24,
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
        )
        .unwrap();
    let request = outbound
        .decode_request(json!({
            "model":"model", "input":[item, {"role":"user","content":"continue"}]
        }))
        .unwrap();
    assert_eq!(
        outbound.encode_request(request).unwrap().body["messages"][0]["content"],
        "Checking hardware."
    );
}

#[test]
fn native_compaction_state_is_dropped_from_requests_responses_and_streams() {
    let outbound = ProtocolTransform::global()
        .bind(
            OPEN_RESPONSES_2026_04_24,
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
        )
        .unwrap();
    for body in [
        json!({"model":"codex","input":[{"type":"compaction","id":"cmp_1","encrypted_content":"opaque"},{"role":"user","content":"hello"}]}),
        json!({"model":"codex","input":[{"type":"compaction_trigger"},{"role":"user","content":"hello"}]}),
        json!({"model":"codex","input":"hello","context_management":[]}),
        json!({"model":"codex","input":"hello","context_management":null}),
    ] {
        let request = outbound.decode_request(body).unwrap();
        let encoded = outbound.encode_request(request).unwrap().body;
        let text = encoded.to_string();
        assert!(!text.contains("opaque") && !text.contains("context_management"));
        assert_eq!(
            encoded["messages"].as_array().unwrap().last().unwrap()["content"],
            "hello"
        );
    }
    let inbound = ProtocolTransform::global()
        .bind(
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            OPEN_RESPONSES_2026_04_24,
        )
        .unwrap();
    let response = inbound
        .decode_response(dated_response(
            "r",
            "completed",
            json!([
                {"type":"compaction","id":"cmp_1","encrypted_content":"opaque"}
            ]),
            Value::Null,
        ))
        .unwrap();
    let encoded = inbound.encode_response(&response).unwrap();
    assert!(!encoded.to_string().contains("opaque"));
    let (_, mut encoder) = inbound.stream().unwrap().into_parts();
    let events = encoder
        .encode_deltas(&[AiStreamDelta::ItemDone {
            index: 0,
            item: response.items[0].clone(),
        }])
        .unwrap();
    assert!(events.iter().all(|event| !event.data.contains("opaque")));
}

#[test]
fn anthropic_compaction_summary_survives_as_text_on_targets_without_the_block() {
    let anthropic_history = json!({
        "model": "claude",
        "max_tokens": 1024,
        "messages": [
            {"role": "assistant", "content": [{"type": "compaction", "content": "Refactored the parser", "signature": "sig-secret"}]},
            {"role": "user", "content": "keep going"}
        ]
    });
    for egress in [
        OPEN_RESPONSES_2026_04_24,
        OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
    ] {
        let pair = ProtocolTransform::global()
            .bind(ANTHROPIC_MESSAGES_2023_06_01, egress)
            .unwrap();
        let request = pair.decode_request(anthropic_history.clone()).unwrap();
        let text = pair.encode_request(request).unwrap().body.to_string();
        assert!(text.contains("Refactored the parser"), "{egress}: {text}");
        assert!(text.contains("keep going"), "{egress}: {text}");
        assert!(!text.contains("sig-secret"), "{egress}: {text}");
        assert!(!text.contains("compaction"), "{egress}: {text}");
    }
}

#[test]
fn anthropic_target_replays_an_anthropic_state_a_responses_client_carried() {
    let state = json!({"type": "compaction", "id": crate::codec::compaction::anthropic_state_id("sig-1"),
    "encrypted_content": crate::codec::compaction::seal_anthropic(&json!({
        "type": "compaction", "content": "Refactored the parser", "signature": "sig-1",
    }))});
    let responses_history = json!({
        "model": "claude",
        "input": [state, {"role": "user", "content": "keep going"}]
    });
    let to_anthropic = ProtocolTransform::global()
        .bind(OPEN_RESPONSES_2026_04_24, ANTHROPIC_MESSAGES_2023_06_01)
        .unwrap();
    let request = to_anthropic
        .decode_request(responses_history.clone())
        .unwrap();
    let body = to_anthropic.encode_request(request).unwrap().body;
    let block = &body["messages"][0]["content"][0];
    assert_eq!(block["type"], "compaction");
    assert_eq!(block["signature"], "sig-1");
    assert_eq!(block["content"], "Refactored the parser");

    let to_chat = ProtocolTransform::global()
        .bind(
            OPEN_RESPONSES_2026_04_24,
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
        )
        .unwrap();
    let request = to_chat.decode_request(responses_history).unwrap();
    let text = to_chat.encode_request(request).unwrap().body.to_string();
    assert!(
        text.contains("Refactored the parser") && !text.contains("sig-1"),
        "{text}"
    );
}

#[test]
fn foreign_target_downgrade_applies_only_when_every_state_is_readable() {
    let pair = ProtocolTransform::global()
        .bind(OPEN_RESPONSES_2026_04_24, OPEN_RESPONSES_2026_04_24)
        .unwrap();
    let anthropic_state = json!({"type": "compaction", "id": crate::codec::compaction::anthropic_state_id("sig-1"),
    "encrypted_content": crate::codec::compaction::seal_anthropic(&json!({
        "type": "compaction", "content": "Summary", "signature": "sig-1",
    }))});
    let responses_state =
        json!({"type": "compaction", "id": "cmp_r", "encrypted_content": "opaque"});

    let mut mixed = pair
        .decode_request(json!({"model": "m", "input": [anthropic_state.clone(), responses_state, {"role": "user", "content": "hi"}]}))
        .unwrap();
    assert!(!super::downgrade_foreign_compaction(&mut mixed));
    assert_eq!(
        mixed
            .items
            .iter()
            .filter(|item| item.is_compaction())
            .count(),
        2
    );

    let mut readable = pair
        .decode_request(
            json!({"model": "m", "input": [anthropic_state, {"role": "user", "content": "hi"}]}),
        )
        .unwrap();
    assert!(super::downgrade_foreign_compaction(&mut readable));
    assert!(readable.items.iter().all(|item| !item.is_compaction()));
}

#[test]
fn thinking_carrier_facts_stay_behind_the_bound_protocol_pair() {
    let responses = ProtocolTransform::global()
        .bind(
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            OPEN_RESPONSES_2026_04_24,
        )
        .expect("registered protocol pair");
    assert_eq!(
        responses.thinking_carrier_facts(),
        ThinkingCarrierFacts {
            indexed: true,
            may_be_protected: true,
            stream_unprotected_summaries: true,
        }
    );

    let anthropic = ProtocolTransform::global()
        .bind(
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            ANTHROPIC_MESSAGES_2023_06_01,
        )
        .expect("registered protocol pair");
    assert_eq!(
        anthropic.thinking_carrier_facts(),
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
        .encode_request(request)
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
        .encode_deltas(&[
            stravia_runtime_contract::protocol::ir::AiStreamDelta::Unknown {
                raw: "event: provider_metadata".to_string(),
            },
        ])
        .expect("same-protocol stream must not apply cross-protocol loss policy");
}

#[test]
fn responses_stream_buffers_utf8_code_point_split_across_transport_chunks() {
    let pair = ProtocolTransform::global()
        .bind(OPEN_RESPONSES_2026_04_24, OPEN_RESPONSES_2026_04_24)
        .expect("registered protocol pair");
    let (mut decoder, _) = pair.stream().expect("stream-capable pair").into_parts();
    let created = crate::codec::open_responses::formatter::response_resource_snapshot(
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
    request.reasoning.target_control = Some(
        stravia_runtime_contract::thinking::TargetThinkingControl::Effort {
            value: "high".into(),
        },
    );

    let encoded = pair
        .encode_request(request)
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
        .encode_request(request)
        .expect("same-protocol rolling extensions");

    assert!(encoded.body.get("client_metadata").is_none());
    assert_eq!(encoded.body["tools"][0]["type"], "namespace");
    assert_eq!(encoded.body["tools"][1]["type"], "web_search");
}

#[test]
fn responses_to_chat_projects_search_without_required_function_choice() {
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

    let body = pair
        .encode_request(request)
        .expect("native search projects independently of function choice")
        .body;

    assert!(body.get("tool_choice").is_none());
    assert_eq!(body["web_search_options"], json!({}));
    assert!(
        body.get("tools")
            .is_none_or(|tools| tools.as_array().is_some_and(Vec::is_empty))
    );
    assert_eq!(body["messages"][0]["content"], "hello");
}

#[test]
fn responses_converts_supported_tool_choices() {
    let cases = [
        (
            ANTHROPIC_MESSAGES_2023_06_01,
            json!("none"),
            json!({"type":"none"}),
        ),
        (
            GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA,
            json!("none"),
            json!({"mode":"NONE"}),
        ),
        (
            GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA,
            json!("required"),
            json!({"mode":"ANY"}),
        ),
        (
            GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA,
            json!({"type": "function", "name": "lookup"}),
            json!({"mode":"ANY", "allowedFunctionNames":["lookup"]}),
        ),
    ];

    for (target, tool_choice, expected) in cases {
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

        let body = pair
            .encode_request(request)
            .expect("a supported tool choice is converted")
            .body;
        if target == ANTHROPIC_MESSAGES_2023_06_01 {
            assert_eq!(body["tool_choice"], expected);
            assert_eq!(body["tools"][0]["name"], "lookup");
        } else {
            assert_eq!(body["toolConfig"]["functionCallingConfig"], expected);
            assert_eq!(
                body["tools"][0]["functionDeclarations"][0]["name"],
                "lookup"
            );
        }
    }
}

#[test]
fn responses_native_search_projects_to_anthropic_and_gemini() {
    for target in [
        ANTHROPIC_MESSAGES_2023_06_01,
        GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA,
    ] {
        let pair = ProtocolTransform::global()
            .bind(OPEN_RESPONSES_2026_04_24, target)
            .unwrap();
        let request = pair
            .decode_request(json!({
                "model": "model",
                "input": "hello",
                "tools": [{"type": "web_search", "filters": {"allowed_domains": ["example.com"]}}],
                "tool_choice": {"type": "web_search"}
            }))
            .unwrap();
        let body = pair.encode_request(request).unwrap().body;
        assert!(body.get("tool_choice").is_none());
        if target == ANTHROPIC_MESSAGES_2023_06_01 {
            assert_eq!(body["tools"][0]["name"], "web_search");
            assert_eq!(body["tools"][0]["allowed_domains"], json!(["example.com"]));
        } else {
            assert_eq!(body["tools"][0]["googleSearch"], json!({}));
        }
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
        .encode_request(request)
        .expect("Gemini AUTO tool selection is representable by OpenAI");

    assert_eq!(encoded.body["tool_choice"], "auto");
    assert_eq!(encoded.body["tools"][0]["function"]["name"], "lookup");
}

#[test]
fn gemini_allowed_function_selection_is_omitted_when_chat_cannot_express_it() {
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

    let body = pair
        .encode_request(request)
        .expect("unsupported allowed selection is omitted")
        .body;

    assert!(body.get("tool_choice").is_none());
    assert_eq!(body["tools"].as_array().map(Vec::len), Some(2));
}

#[test]
fn target_native_continuation_survives_foreign_client_ingress() {
    let native = ProtocolTransform::global()
        .bind(OPEN_RESPONSES_2026_04_24, OPEN_RESPONSES_2026_04_24)
        .unwrap();
    let target_state = native
        .decode_request(json!({
            "model": "model",
            "previous_response_id": "resp-target",
            "input": [{
                "type": "compaction",
                "id": "cmp-target",
                "encrypted_content": "protected-target-state"
            }]
        }))
        .unwrap();
    let responses = ProtocolTransform::global()
        .bind(ANTHROPIC_MESSAGES_2023_06_01, OPEN_RESPONSES_2026_04_24)
        .unwrap();
    let mut request = responses
        .decode_request(json!({
            "model": "model",
            "max_tokens": 128,
            "messages": [{"role": "user", "content": "continue"}]
        }))
        .unwrap();
    request.parameters.continuation = target_state.parameters.continuation;
    request.items.splice(0..0, target_state.items);

    let body = responses.encode_request(request.clone()).unwrap().body;
    assert_eq!(body["previous_response_id"], "resp-target");
    assert_eq!(body["input"][0]["type"], "compaction");
    assert_eq!(
        body["input"][0]["encrypted_content"],
        "protected-target-state"
    );
    assert_eq!(body["input"][1]["content"][0]["text"], "continue");

    let chat = ProtocolTransform::global()
        .bind(
            ANTHROPIC_MESSAGES_2023_06_01,
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
        )
        .unwrap();
    let body = chat.encode_request(request).unwrap().body;
    assert!(body.get("previous_response_id").is_none());
    assert!(!body.to_string().contains("protected-target-state"));
    assert_eq!(body["messages"][0]["content"], "continue");
}

#[test]
fn responses_cross_protocol_drops_controls_the_target_cannot_carry() {
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
    let encoded = pair
        .encode_request(request)
        .expect("shared controls project to Chat")
        .body;
    assert_eq!(encoded["prompt_cache_key"], "cache-key");
    assert_eq!(encoded["service_tier"], "default");
    assert_eq!(encoded["metadata"], json!({"tenant": "local"}));
    assert_eq!(encoded["safety_identifier"], "local-user");
    assert_eq!(encoded["store"], false);
    assert_eq!(encoded["top_logprobs"], 3);
    assert!(encoded.get("include").is_none());

    for (field, value) in [("max_tool_calls", json!(2)), ("truncation", json!("auto"))] {
        let mut body = json!({
            "model": "chat-model",
            "input": "hello"
        });
        body[field] = value;
        let request = pair.decode_request(body).expect("valid Responses request");
        let encoded = pair
            .encode_request(request)
            .expect("a control the target cannot carry is dropped")
            .body;
        assert!(encoded.get(field).is_none(), "{field}");
        assert_eq!(encoded["messages"][0]["content"], "hello");
    }
}

#[test]
fn responses_disabled_truncation_preserves_anthropic_tool_continuation() {
    let pair = ProtocolTransform::global()
        .bind(OPEN_RESPONSES_2026_04_24, ANTHROPIC_MESSAGES_2023_06_01)
        .expect("registered protocol pair");
    let request = pair
        .decode_request(json!({
            "model": "claude-sonnet-5-5",
            "truncation": "disabled",
            "max_output_tokens": 128,
            "input": [
                {"role": "user", "content": "retain the first turn"},
                {"type": "function_call", "call_id": "call_read", "name": "read", "arguments": "{\"path\":\"a\"}"},
                {"type": "function_call_output", "call_id": "call_read", "output": "retained tool result"},
                {"role": "user", "content": "continue with all history"}
            ]
        }))
        .expect("valid Responses request");
    let encoded = pair
        .encode_request(request)
        .expect("Anthropic preserves complete input when truncation is disabled");
    assert_eq!(
        encoded.body["messages"],
        json!([
            {"role": "user", "content": [{"type": "text", "text": "retain the first turn"}]},
            {"role": "assistant", "content": [{"type": "tool_use", "id": "call_read", "name": "read", "input": {"path": "a"}}]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "call_read", "content": "retained tool result"},
                {"type": "text", "text": "continue with all history"}
            ]}
        ])
    );
}

#[test]
fn responses_auto_truncation_is_omitted_for_anthropic() {
    let pair = ProtocolTransform::global()
        .bind(OPEN_RESPONSES_2026_04_24, ANTHROPIC_MESSAGES_2023_06_01)
        .expect("registered protocol pair");
    let request = pair
        .decode_request(json!({
            "model": "claude-sonnet-5-5",
            "input": "retain context handling semantics",
            "truncation": "auto"
        }))
        .expect("valid Responses request");
    let body = pair.encode_request(request).unwrap().body;
    assert!(body.get("truncation").is_none());
    assert_eq!(
        body["messages"][0]["content"][0]["text"],
        "retain context handling semantics"
    );
}

#[test]
fn pair_bound_adapter_omits_unsupported_candidate_count() {
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

    let body = pair.encode_request(request).unwrap().body;
    assert!(body.get("n").is_none());
    assert_eq!(body["messages"][0]["content"][0]["text"], "compare");
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
        .encode_request(request)
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
        .encode_request(request)
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
        .encode_request(request)
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
        .encode_request(request)
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
        .encode_request(request)
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
            "input": [
                {
                    "type": "function_call",
                    "call_id": "call_123",
                    "name": "render_chart",
                    "arguments": "{}"
                },
                {
                    "type": "function_call_output",
                    "call_id": "call_123",
                    "output": [
                        {"type": "input_text", "text": "chart"},
                        {"type": "input_image", "image_url": "https://example.test/chart.png"}
                    ]
                }
            ],
            "max_output_tokens": 128
        }))
        .expect("valid Responses function output");

    let encoded = pair
        .encode_request(request)
        .expect("representable Anthropic tool result");
    let blocks = encoded.body["messages"]
        .as_array()
        .expect("Anthropic messages")
        .iter()
        .filter_map(|message| message["content"].as_array())
        .flatten()
        .collect::<Vec<_>>();
    let call = blocks
        .iter()
        .find(|block| block["type"] == "tool_use")
        .expect("Anthropic tool use");
    let result = blocks
        .iter()
        .find(|block| block["type"] == "tool_result")
        .expect("Anthropic tool result");

    assert_eq!(call["id"], "call_123");
    assert_eq!(result["tool_use_id"], call["id"]);
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
        .encode_request(request)
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
    let created = crate::codec::open_responses::formatter::response_resource_snapshot(
        "resp_partial",
        "gpt-5.4",
        "in_progress",
        Vec::new(),
        Value::Null,
        Value::Null,
        Value::Null,
    );
    let incomplete = crate::codec::open_responses::formatter::response_resource_snapshot(
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
    let created = crate::codec::open_responses::formatter::response_resource_snapshot(
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
fn canonical_video_is_native_for_gemini_and_omitted_for_anthropic() {
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
        .encode_request(request.clone())
        .expect("Gemini native video");
    assert_eq!(
        gemini.body["contents"][0]["parts"][0]["fileData"]["mimeType"],
        "video/mp4"
    );

    for source in [
        MediaSource::Url("https://example.com/video.mp4".into()),
        MediaSource::Base64 {
            media_type: "video/mp4".into(),
            data: "dmlkZW8=".into(),
        },
    ] {
        let mut native_request = request.clone();
        native_request.items[0].content = MessageContent::Blocks(vec![ContentBlock::Video {
            source: source.clone(),
            media_type: Some("video/mp4".into()),
        }]);
        let responses = ProtocolTransform::global()
            .bind(
                GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA,
                OPEN_RESPONSES_2026_04_24,
            )
            .unwrap()
            .encode_request(native_request)
            .unwrap();
        let video = &responses.body["input"][0]["content"][0];
        assert_eq!(video["type"], "input_video");
        let expected = match source {
            MediaSource::Url(url) => url,
            MediaSource::Base64 { media_type, data } => format!("data:{media_type};base64,{data}"),
            _ => unreachable!(),
        };
        assert_eq!(video["video_url"], expected);
    }

    let encoded = ProtocolTransform::global()
        .bind(
            GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA,
            ANTHROPIC_MESSAGES_2023_06_01,
        )
        .unwrap()
        .encode_request(request)
        .unwrap();
    assert!(!encoded.body.to_string().contains("video.mp4"));
}
#[test]
fn responses_reasoning_before_tool_call_is_preserved_for_openai_compatible() {
    let request = AiRequest::new(
        "reasoning-model",
        vec![
            AiItem {
                role: Role::User,
                content: MessageContent::Text("inspect".to_owned().into()),
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
                content: MessageContent::Text("{\"completion\":\"complete\"}".to_owned().into()),
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
        .encode_request(request)
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
        .encode_request(request)
        .expect("canonical reasoning is native Open Responses input");

    assert_eq!(encoded.body["input"][0]["type"], "reasoning");
    assert_eq!(encoded.body["input"][0]["summary"][0]["text"], "summary");
    assert_eq!(encoded.body["input"][0]["encrypted_content"], "opaque");
}

#[test]
fn strict_function_tool_keeps_supported_strict_and_schema_constraints() {
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
        let encoded = ProtocolTransform::global()
            .bind(OPEN_RESPONSES_2026_04_24, target)
            .expect("registered target pair")
            .encode_request(request.clone())
            .expect("function tool strict is an advisory hint");
        let tool = &encoded.body["tools"][0];
        if target == ANTHROPIC_MESSAGES_2023_06_01 {
            assert_eq!(tool["name"], "lookup");
            assert_eq!(tool["strict"], true);
            assert_eq!(tool["input_schema"]["additionalProperties"], false);
        } else {
            let declaration = &tool["functionDeclarations"][0];
            assert_eq!(declaration["name"], "lookup");
            assert!(declaration.get("strict").is_none());
            assert_eq!(
                declaration["parametersJsonSchema"]["additionalProperties"],
                false
            );
        }
    }
}
/// Chat 与 Anthropic 省略 `strict` 表示非严格，而 Responses 省略表示由上游自动严格化。
/// 跨协议到 Responses 时必须显式写出非严格，否则可选参数会被上游改成必填。
#[test]
fn responses_target_keeps_non_strict_default_of_other_ingress_protocols() {
    let tools = json!([{
        "type": "function",
        "function": {
            "name": "lookup",
            "parameters": {
                "type": "object",
                "properties": {"query": {"type": "string"}, "note": {"type": "string"}},
                "required": ["query"]
            }
        }
    }]);
    let chat = ProtocolTransform::global()
        .bind(
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            OPEN_RESPONSES_2026_04_24,
        )
        .expect("registered chat pair");
    let request = chat
        .decode_request(json!({
            "model": "logical-model",
            "messages": [{"role": "user", "content": "hello"}],
            "tools": tools,
        }))
        .expect("valid chat request");
    let encoded = chat.encode_request(request).expect("encode Responses");
    assert_eq!(encoded.body["tools"][0]["strict"], false);

    let anthropic = ProtocolTransform::global()
        .bind(ANTHROPIC_MESSAGES_2023_06_01, OPEN_RESPONSES_2026_04_24)
        .expect("registered anthropic pair");
    let request = anthropic
        .decode_request(json!({
            "model": "logical-model",
            "max_tokens": 64,
            "messages": [{"role": "user", "content": "hello"}],
            "tools": [{
                "name": "lookup",
                "input_schema": {
                    "type": "object",
                    "properties": {"query": {"type": "string"}, "note": {"type": "string"}},
                    "required": ["query"]
                }
            }],
        }))
        .expect("valid anthropic request");
    let encoded = anthropic.encode_request(request).expect("encode Responses");
    assert_eq!(encoded.body["tools"][0]["strict"], false);

    let explicit = chat
        .decode_request(json!({
            "model": "logical-model",
            "messages": [{"role": "user", "content": "hello"}],
            "tools": [{
                "type": "function",
                "function": {
                    "name": "lookup",
                    "strict": true,
                    "parameters": {"type": "object", "properties": {}, "additionalProperties": false}
                }
            }],
        }))
        .expect("valid strict chat request");
    let encoded = chat.encode_request(explicit).expect("encode Responses");
    assert_eq!(encoded.body["tools"][0]["strict"], true);
}

#[test]
fn chat_target_preserves_explicit_strict_and_omits_unset() {
    let pair = ProtocolTransform::global()
        .bind(
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
        )
        .expect("registered chat pair");
    let request = pair
        .decode_request(json!({
            "model": "logical-model",
            "messages": [{"role": "user", "content": "hello"}],
            "tools": [
                {"type": "function", "function": {"name": "on", "strict": true, "parameters": {"type": "object"}}},
                {"type": "function", "function": {"name": "off", "strict": false, "parameters": {"type": "object"}}},
                {"type": "function", "function": {"name": "unset", "parameters": {"type": "object"}}}
            ],
        }))
        .expect("valid chat request");
    let encoded = pair.encode_request(request).expect("encode chat");
    assert_eq!(encoded.body["tools"][0]["function"]["strict"], true);
    assert_eq!(encoded.body["tools"][1]["function"]["strict"], false);
    assert!(encoded.body["tools"][2]["function"].get("strict").is_none());
}

#[test]
fn responses_ingress_keeps_omitted_strict_for_responses_target() {
    let pair = ProtocolTransform::global()
        .bind(OPEN_RESPONSES_2026_04_24, OPEN_RESPONSES_2026_04_24)
        .expect("registered Responses pair");
    let request = pair
        .decode_request(json!({
            "model": "logical-model",
            "input": "hello",
            "tools": [{
                "type": "function",
                "name": "lookup",
                "parameters": {"type": "object", "properties": {}}
            }]
        }))
        .expect("valid Responses request");
    let encoded = pair.encode_request(request).expect("encode Responses");
    assert!(encoded.body["tools"][0].get("strict").is_none());
}

#[test]
fn chat_ingress_keeps_refusal_apart_from_text() {
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
            text: "visible".to_owned().into(),
            cache_control: None,
        },
        ContentBlock::Refusal {
            refusal: "cannot comply".into(),
        },
    ]);
    response.items.push(mixed);

    let encoded = pair.encode_response(&response).unwrap();
    assert_eq!(encoded["choices"][0]["message"]["content"], "visible");
    assert_eq!(encoded["choices"][0]["message"]["refusal"], "cannot comply");
    let (_, mut encoder) = pair.stream().unwrap().into_parts();
    let events = encoder
        .encode_deltas(&[
            AiStreamDelta::TextDelta("visible".into()),
            AiStreamDelta::RefusalDelta("cannot comply".into()),
        ])
        .unwrap();
    let deltas: Vec<Value> = events
        .iter()
        .map(|event| {
            serde_json::from_str::<Value>(&event.data).unwrap()["choices"][0]["delta"].clone()
        })
        .collect();
    assert!(deltas.iter().any(|delta| delta["content"] == "visible"));
    assert!(
        deltas
            .iter()
            .any(|delta| delta["refusal"] == "cannot comply")
    );
    assert!(
        deltas
            .iter()
            .all(|delta| delta["content"] != "cannot comply")
    );
}

const ANTHROPIC_REFUSAL_EXPLANATION: &str = "This request was blocked by an upstream usage policy.";
// 凭据哨兵：上游 stop_details 携带的 fallback_credit_token 绝不能出现在任何客户端输出中。
const ANTHROPIC_REFUSAL_CREDIT_SENTINEL: &str = "credit-token-sentinel-7f3a";

fn anthropic_refusal_details() -> Value {
    json!({
        "type": "refusal",
        "category": "reasoning_extraction",
        "explanation": ANTHROPIC_REFUSAL_EXPLANATION,
        "fallback_credit_token": ANTHROPIC_REFUSAL_CREDIT_SENTINEL,
        "fallback_has_prefill_claim": false,
    })
}

/// 上游 Anthropic 拒答：HTTP 200、无内容块、输出 token 为 0。
fn anthropic_refusal_unary(details: Option<Value>) -> Value {
    let mut body = json!({
        "id": "msg_refused",
        "type": "message",
        "role": "assistant",
        "model": "claude-opus-5-5",
        "content": [],
        "stop_reason": "refusal",
        "stop_sequence": null,
        "usage": {"input_tokens": 2, "output_tokens": 0},
    });
    if let Some(details) = details {
        body["stop_details"] = details;
    }
    body
}

fn anthropic_refusal_stream(details: Option<Value>) -> String {
    let mut delta = json!({"stop_reason": "refusal", "stop_sequence": null});
    if let Some(details) = details {
        delta["stop_details"] = details;
    }
    let start = json!({
        "type": "message_start",
        "message": {
            "id": "msg_refused", "type": "message", "role": "assistant",
            "model": "claude-opus-5-5", "content": [], "stop_reason": null,
            "usage": {"input_tokens": 2, "output_tokens": 0},
        },
    });
    let message_delta = json!({
        "type": "message_delta",
        "delta": delta,
        "usage": {"input_tokens": 2, "output_tokens": 0},
    });
    format!(
        "event: message_start\ndata: {start}\n\n\
         event: message_delta\ndata: {message_delta}\n\n\
         event: message_stop\ndata: {{\"type\":\"message_stop\"}}\n\n"
    )
}

fn refused_unary_through(ingress: super::ProtocolEndpoint, details: Option<Value>) -> Value {
    let pair = ProtocolTransform::global()
        .bind(ingress, ANTHROPIC_MESSAGES_2023_06_01)
        .expect("registered protocol pair");
    let response = pair
        .decode_response(anthropic_refusal_unary(details))
        .expect("refusal is a valid Anthropic response");
    pair.encode_response(&response)
        .expect("refusal is representable by the ingress")
}

fn refused_stream_through(
    ingress: super::ProtocolEndpoint,
    details: Option<Value>,
) -> Vec<(Option<String>, Value)> {
    let pair = ProtocolTransform::global()
        .bind(ingress, ANTHROPIC_MESSAGES_2023_06_01)
        .expect("registered protocol pair");
    let (mut decoder, mut encoder) = pair.stream().expect("stream-capable pair").into_parts();
    let mut deltas = decoder
        .decode_chunk(anthropic_refusal_stream(details).as_bytes())
        .expect("refusal is a valid Anthropic stream");
    deltas.extend(decoder.finish().expect("stream finishes"));
    let mut events = encoder
        .encode_deltas(&deltas)
        .expect("refusal is representable by the ingress");
    events.extend(encoder.finish().expect("encoder finishes"));
    events
        .into_iter()
        .filter(|event| event.data != "[DONE]")
        .map(|event| {
            let payload = serde_json::from_str(&event.data).expect("event data is JSON");
            (event.event, payload)
        })
        .collect()
}

fn assert_no_credit_token(wire: &impl std::fmt::Display) {
    assert!(
        !wire.to_string().contains(ANTHROPIC_REFUSAL_CREDIT_SENTINEL),
        "fallback_credit_token must never reach the client"
    );
}

#[test]
fn anthropic_refusal_reaches_responses_client() {
    let encoded =
        refused_unary_through(OPEN_RESPONSES_2026_04_24, Some(anthropic_refusal_details()));
    assert_eq!(encoded["status"], "completed");
    let refusal = encoded["output"]
        .as_array()
        .and_then(|output| output.iter().find(|item| item["type"] == "message"))
        .and_then(|message| message["content"].as_array())
        .and_then(|content| content.iter().find(|part| part["type"] == "refusal"))
        .expect("a refusal content part, not an empty output");
    assert_eq!(refusal["refusal"], ANTHROPIC_REFUSAL_EXPLANATION);
    assert_no_credit_token(&encoded);

    let events =
        refused_stream_through(OPEN_RESPONSES_2026_04_24, Some(anthropic_refusal_details()));
    let done = events
        .iter()
        .find(|(name, _)| name.as_deref() == Some("response.refusal.done"))
        .expect("streamed refusal part");
    assert_eq!(done.1["refusal"], ANTHROPIC_REFUSAL_EXPLANATION);
    let completed = events
        .iter()
        .find(|(name, _)| name.as_deref() == Some("response.completed"))
        .expect("the refusal is a terminal completed response");
    assert_eq!(completed.1["response"]["status"], "completed");
    assert_eq!(
        completed.1["response"]["output"][0]["content"][0]["refusal"],
        ANTHROPIC_REFUSAL_EXPLANATION
    );
    assert_no_credit_token(&Value::Array(events.into_iter().map(|(_, v)| v).collect()));
}

#[test]
fn anthropic_refusal_reaches_chat_client() {
    let encoded = refused_unary_through(
        OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
        Some(anthropic_refusal_details()),
    );
    assert_eq!(encoded["choices"][0]["finish_reason"], "content_filter");
    assert_eq!(
        encoded["choices"][0]["message"]["refusal"],
        ANTHROPIC_REFUSAL_EXPLANATION
    );
    assert_no_credit_token(&encoded);

    let events = refused_stream_through(
        OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
        Some(anthropic_refusal_details()),
    );
    let choices: Vec<&Value> = events
        .iter()
        .map(|(_, chunk)| &chunk["choices"][0])
        .collect();
    assert!(
        choices
            .iter()
            .any(|choice| choice["delta"]["refusal"] == ANTHROPIC_REFUSAL_EXPLANATION),
        "refusal text must be streamed in delta.refusal"
    );
    assert!(
        choices
            .iter()
            .all(|choice| choice["delta"].get("content").is_none()),
        "refusal is not assistant content"
    );
    assert_eq!(
        choices.last().expect("terminal chunk")["finish_reason"],
        "content_filter"
    );
    assert_no_credit_token(&Value::Array(events.into_iter().map(|(_, v)| v).collect()));
}

#[test]
fn anthropic_refusal_reaches_anthropic_client() {
    let encoded = refused_unary_through(
        ANTHROPIC_MESSAGES_2023_06_01,
        Some(anthropic_refusal_details()),
    );
    assert_eq!(encoded["stop_reason"], "refusal");
    assert_eq!(
        encoded["content"],
        json!([{"type": "text", "text": ANTHROPIC_REFUSAL_EXPLANATION}])
    );
    assert_no_credit_token(&encoded);

    let events = refused_stream_through(
        ANTHROPIC_MESSAGES_2023_06_01,
        Some(anthropic_refusal_details()),
    );
    let text: String = events
        .iter()
        .filter(|(name, _)| name.as_deref() == Some("content_block_delta"))
        .filter_map(|(_, payload)| payload["delta"]["text"].as_str())
        .collect();
    assert_eq!(text, ANTHROPIC_REFUSAL_EXPLANATION);
    let terminal = events
        .iter()
        .find(|(name, _)| name.as_deref() == Some("message_delta"))
        .expect("terminal message_delta");
    assert_eq!(terminal.1["delta"]["stop_reason"], "refusal");
    assert_no_credit_token(&Value::Array(events.into_iter().map(|(_, v)| v).collect()));
}

#[test]
fn anthropic_refusal_without_details_still_reaches_every_client() {
    let responses = refused_unary_through(OPEN_RESPONSES_2026_04_24, None);
    let refusal = responses["output"][0]["content"][0]["refusal"]
        .as_str()
        .expect("Responses client gets a refusal part");
    assert!(!refusal.is_empty());

    let chat = refused_unary_through(OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1, None);
    assert_eq!(chat["choices"][0]["finish_reason"], "content_filter");
    assert!(
        chat["choices"][0]["message"]["refusal"]
            .as_str()
            .is_some_and(|text| !text.is_empty())
    );

    let anthropic = refused_unary_through(ANTHROPIC_MESSAGES_2023_06_01, None);
    assert_eq!(anthropic["stop_reason"], "refusal");
    assert!(
        anthropic["content"][0]["text"]
            .as_str()
            .is_some_and(|text| !text.is_empty())
    );
}

#[test]
fn cross_protocol_native_alternates_are_first_only_without_private_leaks() {
    let chat = crate::codec::openai::compatible::stream::OpenAIResponseParser
        .parse_response(json!({"choices": [{"message": {"content": "first"}}, {"index": 1, "message": {"content": "alternate"}}]})).unwrap();
    let gemini = crate::codec::google::gemini::stream::GoogleResponseParser
        .parse_response(json!({"candidates": [{"content": {"parts": [{"text": "first"}]}}, {"index": 1, "content": {"parts": [{"text": "alternate"}]}}]})).unwrap();
    for (source, target, response, marker) in [
        (
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA,
            chat,
            "__openai_native_choices",
        ),
        (
            GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA,
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            gemini,
            "__google_native_candidates",
        ),
    ] {
        let pair = ProtocolTransform::global().bind(target, source).unwrap();
        let encoded = pair.encode_response(&response).unwrap();
        assert!(encoded.to_string().contains("first"));
        assert!(!encoded.to_string().contains("alternate"));
        assert!(!encoded.to_string().contains(marker));
        let (_, mut encoder) = pair.stream().unwrap().into_parts();
        let events = encoder
            .encode_deltas(&[
                AiStreamDelta::TextDelta("first".into()),
                AiStreamDelta::Unknown {
                    raw: json!({(marker): [{"index": 1, "text": "alternate"}]}).to_string(),
                },
                AiStreamDelta::Usage(stravia_runtime_contract::protocol::ir::usage::Usage {
                    prompt_tokens: 2,
                    completion_tokens: 3,
                    total_tokens: 5,
                    required_components_known: true,
                    ..Default::default()
                }),
                AiStreamDelta::Done {
                    stop_reason: "stop".into(),
                },
            ])
            .unwrap();
        assert!(events.iter().any(|event| event.data.contains("first")));
        assert!(
            events
                .iter()
                .all(|event| !event.data.contains("alternate") && !event.data.contains(marker))
        );
        assert!(
            events
                .iter()
                .any(|event| event.data.contains("promptTokenCount")
                    || event.data.contains("prompt_tokens"))
        );
    }
}

#[test]
fn chat_response_preserves_logprobs_but_omits_responses_annotations() {
    let pair = ProtocolTransform::global()
        .bind(
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            OPEN_RESPONSES_2026_04_24,
        )
        .expect("registered protocol pair");
    let mut item = AiItem::output_text("answer");
    item.meta = Some(
        stravia_runtime_contract::protocol::ir::AiItemMetadata::boxed(json!({
            "__open_responses_content": [{
                "type": "output_text",
                "text": "answer",
                "annotations": [{"type": "url_citation", "url": "https://example.test"}],
                "logprobs": [{"token": "answer", "logprob": -0.1}]
            }]
        })),
    );
    let mut response = AiResponse::new("resp-metadata", "logical-model");
    response.items.push(item);

    let encoded = pair.encode_response(&response).unwrap();
    assert_eq!(encoded["choices"][0]["message"]["content"], "answer");
    assert!(!encoded.to_string().contains("url_citation"));
    assert_eq!(
        encoded["choices"][0]["logprobs"]["content"][0]["token"],
        "answer"
    );
}

#[test]
fn dated_stream_metadata_preserves_text_and_omits_unsupported_metadata() {
    let pair = ProtocolTransform::global()
        .bind(
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            OPEN_RESPONSES_2026_04_24,
        )
        .expect("registered protocol pair");

    let (_, mut encoder) = pair.stream().unwrap().into_parts();
    let events = encoder
        .encode_deltas(&[
            AiStreamDelta::MessageStart {
                id: "chat-pad-metadata".into(),
                model: "logical-model".into(),
            },
            AiStreamDelta::TextDeltaWithMetadata {
                text: "answer".into(),
                logprobs: vec![json!({"token":"answer"})],
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
        ])
        .unwrap();
    let chunks: Vec<Value> = events
        .iter()
        .map(|event| serde_json::from_str(&event.data).expect("Chat chunk JSON"))
        .collect();
    let text = chunks
        .iter()
        .find(|chunk| chunk["choices"][0]["delta"]["content"] == "answer")
        .expect("preserved text delta");
    assert_eq!(
        text["choices"][0]["logprobs"]["content"],
        json!([{"token": "answer"}])
    );
    assert!(
        chunks
            .iter()
            .any(|chunk| chunk["choices"][0]["delta"]["reasoning_content"] == "summary")
    );
    for chunk in &chunks {
        assert!(chunk.get("obfuscation").is_none());
        assert!(chunk["choices"][0].get("obfuscation").is_none());
        assert!(chunk["choices"][0]["delta"].get("obfuscation").is_none());
    }
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
fn gemini_thought_and_idless_function_call_are_lossless_for_openai_clients() {
    let pair = ProtocolTransform::global()
        .bind(
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA,
        )
        .expect("registered protocol pair");
    let response = pair
        .decode_response(json!({
            "responseId": "gemini-contract",
            "modelVersion": "upstream-model",
            "candidates": [{
                "content": {"role": "model", "parts": [
                    {"text": "because", "thought": true},
                    {"functionCall": {"name": "lookup", "args": {"key": "answer"}}}
                ]},
                "finishReason": "STOP"
            }],
            "usageMetadata": {
                "promptTokenCount": 3,
                "candidatesTokenCount": 4,
                "thoughtsTokenCount": 2,
                "totalTokenCount": 7
            }
        }))
        .expect("valid Gemini response");

    let encoded = pair
        .encode_response(&response)
        .expect("Gemini response is representable for an OpenAI client");
    assert_eq!(
        encoded["choices"][0]["message"]["reasoning_content"],
        "because"
    );
    assert_eq!(
        encoded["choices"][0]["message"]["tool_calls"][0]["function"]["name"],
        "lookup"
    );
    assert_eq!(
        encoded["choices"][0]["message"]["tool_calls"][0]["function"]["arguments"],
        r#"{"key":"answer"}"#
    );
    assert!(
        encoded["choices"][0]["message"]["tool_calls"][0]["id"]
            .as_str()
            .is_some_and(|id| !id.is_empty())
    );
    assert_eq!(encoded["choices"][0]["finish_reason"], "tool_calls");
    assert_eq!(encoded["usage"]["prompt_tokens"], 3);
    assert_eq!(encoded["usage"]["completion_tokens"], 4);
}

#[test]
fn gemini_stream_accepts_responses_reasoning_summary() {
    let pair = ProtocolTransform::global()
        .bind(
            GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA,
            OPEN_RESPONSES_2026_04_24,
        )
        .expect("registered protocol pair");

    let (_, mut encoder) = pair.stream().unwrap().into_parts();
    let events = encoder
        .encode_deltas(&[AiStreamDelta::ReasoningSummaryDelta {
            text: "summary".into(),
            obfuscation: None,
            output_index: Some(0),
            content_index: Some(0),
        }])
        .unwrap();
    assert!(events.iter().any(|event| event.data.contains("summary")));
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

    let encoded = pair
        .encode_request(request)
        .expect("advisory controls are omitted");
    assert_eq!(
        encoded.body["messages"][0]["content"],
        json!([{"type":"text","text":"hello"}])
    );
    assert!(encoded.body.get("stream_options").is_none());
    assert!(encoded.body.get("text").is_none());
}

#[test]
fn canonical_controls_project_across_chat_and_gemini_without_native_leaks() {
    let pair = ProtocolTransform::global()
        .bind(
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA,
        )
        .expect("registered Chat to Gemini pair");
    let mut request = pair
        .decode_request(json!({
            "model": "model", "messages": [{"role": "user", "content": "hello"}],
            "n": 0, "logprobs": true, "top_logprobs": 0, "modalities": ["text", "audio"],
            "service_tier": "priority", "unknown_provider_control": {"private": true}
        }))
        .expect("decode platform controls");
    request.parameters.candidate_count = Some(2);
    let encoded = pair
        .encode_request(request)
        .expect("project platform controls");
    assert_eq!(encoded.body["generationConfig"]["candidateCount"], 2);
    assert_eq!(encoded.body["generationConfig"]["responseLogprobs"], true);
    assert_eq!(encoded.body["generationConfig"]["logprobs"], 0);
    assert_eq!(
        encoded.body["generationConfig"]["responseModalities"],
        json!(["TEXT", "AUDIO"])
    );
    assert!(encoded.body.get("cachedContent").is_none());
    assert!(encoded.body.get("service_tier").is_none());
    assert!(encoded.body.get("unknown_provider_control").is_none());
}

#[test]
fn responses_stream_and_text_controls_project_to_chat_canonical_fields() {
    let pair = ProtocolTransform::global()
        .bind(
            OPEN_RESPONSES_2026_04_24,
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
        )
        .expect("registered Responses to Chat pair");
    let request = pair
        .decode_request(json!({
            "model": "model", "input": "hello", "stream": true,
            "stream_options": {"include_obfuscation": false},
            "text": {"format": {"type": "text"}, "verbosity": "low"},
            "prompt_cache_key": "", "prompt_cache_retention": "24h",
            "background": false, "unknown_provider_control": true
        }))
        .expect("decode canonical stream controls");
    let encoded = pair
        .encode_request(request)
        .expect("project shared controls")
        .body;
    assert!(
        encoded["stream_options"]
            .get("include_obfuscation")
            .is_none()
    );
    assert_eq!(encoded["verbosity"], "low");
    assert_eq!(encoded["prompt_cache_key"], "");
    assert_eq!(encoded["prompt_cache_retention"], "24h");
    assert!(encoded.get("text").is_none());
    assert!(encoded.get("background").is_none());
    assert!(encoded.get("unknown_provider_control").is_none());
}

#[test]
fn open_responses_client_omits_unknown_provider_output_items() {
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

    response.items.push(AiItem::output_text("preserved answer"));
    let encoded = pair.encode_response(&response).unwrap();
    assert_eq!(
        encoded["output"][0]["content"][0]["text"],
        "preserved answer"
    );
    assert!(!encoded.to_string().contains("executable_code"));
}

#[test]
fn image_detail_is_ignored_without_losing_images_on_targets_without_the_control() {
    for ingress in [
        OPEN_RESPONSES_2026_04_24,
        OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
    ] {
        for target in [
            ANTHROPIC_MESSAGES_2023_06_01,
            GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA,
        ] {
            let pair = ProtocolTransform::global()
                .bind(ingress, target)
                .expect("registered protocol pair");
            let details: &[&str] = if ingress == OPEN_RESPONSES_2026_04_24 {
                &["auto", "low", "high"]
            } else {
                &["auto", "low", "high", "original", "future"]
            };
            for detail in details {
                let body = if ingress == OPEN_RESPONSES_2026_04_24 {
                    json!({
                        "model": "model",
                        "input": [{
                            "role": "user",
                            "content": [{
                                "type": "input_image",
                                "image_url": "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR4nGP4z8AAAAMBAQDJ/pLvAAAAAElFTkSuQmCC",
                                "detail": detail
                            }]
                        }]
                    })
                } else {
                    json!({
                        "model": "model",
                        "messages": [{
                            "role": "user",
                            "content": [{
                                "type": "image_url",
                                "image_url": {
                                    "url": "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR4nGP4z8AAAAMBAQDJ/pLvAAAAAElFTkSuQmCC",
                                    "detail": detail
                                }
                            }]
                        }]
                    })
                };
                let request = pair.decode_request(body).expect("image request");
                let body = pair
                    .encode_request(request)
                    .expect("unsupported image resolution hints do not block requests")
                    .body;

                if target == ANTHROPIC_MESSAGES_2023_06_01 {
                    assert_eq!(
                        body["messages"][0]["content"],
                        json!([{
                            "type": "image",
                            "source": {
                                "type": "base64",
                                "media_type": "image/png",
                                "data": "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR4nGP4z8AAAAMBAQDJ/pLvAAAAAElFTkSuQmCC"
                            }
                        }])
                    );
                } else {
                    assert_eq!(
                        body["contents"][0]["parts"],
                        json!([{
                            "inlineData": {
                                "mimeType": "image/png",
                                "data": "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR4nGP4z8AAAAMBAQDJ/pLvAAAAAElFTkSuQmCC"
                            }
                        }])
                    );
                }
            }
        }
    }
}

#[test]
fn image_detail_does_not_block_anthropic_tool_continuation() {
    let pair = ProtocolTransform::global()
        .bind(OPEN_RESPONSES_2026_04_24, ANTHROPIC_MESSAGES_2023_06_01)
        .expect("registered protocol pair");
    let request = pair
        .decode_request(json!({
            "model": "model",
            "input": [
                {"type": "function_call", "call_id": "call_1", "name": "screenshot", "arguments": "{}"},
                {"type": "function_call_output", "call_id": "call_1", "output": [
                    {"type": "input_text", "text": "Screenshot captured"},
                    {"type": "input_image", "image_url": "data:image/webp;base64,d2VicA==", "detail": "auto"}
                ]}
            ]
        }))
        .expect("image tool result");
    let body = pair
        .encode_request(request)
        .expect("image tool continuation")
        .body;
    assert_eq!(
        body["messages"][1]["content"],
        json!([{
            "type": "tool_result",
            "tool_use_id": "call_1",
            "content": [
                {"type": "text", "text": "Screenshot captured"},
                {"type": "image", "source": {"type": "base64", "media_type": "image/webp", "data": "d2VicA=="}}
            ]
        }])
    );
}
#[test]
fn image_detail_is_preserved_on_openai_targets() {
    for ingress in [
        OPEN_RESPONSES_2026_04_24,
        OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
    ] {
        for target in [
            OPEN_RESPONSES_2026_04_24,
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
        ] {
            let pair = ProtocolTransform::global()
                .bind(ingress, target)
                .expect("registered protocol pair");
            for detail in ["auto", "low", "high"] {
                let body = if ingress == OPEN_RESPONSES_2026_04_24 {
                    json!({
                        "model": "model",
                        "input": [{
                            "role": "user",
                            "content": [{
                                "type": "input_image",
                                "image_url": "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR4nGP4z8AAAAMBAQDJ/pLvAAAAAElFTkSuQmCC",
                                "detail": detail
                            }]
                        }]
                    })
                } else {
                    json!({
                        "model": "model",
                        "messages": [{
                            "role": "user",
                            "content": [{
                                "type": "image_url",
                                "image_url": {
                                    "url": "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR4nGP4z8AAAAMBAQDJ/pLvAAAAAElFTkSuQmCC",
                                    "detail": detail
                                }
                            }]
                        }]
                    })
                };
                let request = pair.decode_request(body).expect("image request");
                let body = pair.encode_request(request).expect("supported hint").body;
                let encoded_detail = if target == OPEN_RESPONSES_2026_04_24 {
                    &body["input"][0]["content"][0]["detail"]
                } else {
                    &body["messages"][0]["content"][0]["image_url"]["detail"]
                };
                assert_eq!(encoded_detail, detail);
            }
        }
    }
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

    let encoded = pair
        .encode_request(request)
        .expect("lossless Gemini structured output");
    assert_eq!(
        encoded.body["generationConfig"]["responseMimeType"],
        "application/json"
    );
    assert_eq!(
        encoded.body["generationConfig"]["responseJsonSchema"]["properties"]["answer"]["type"],
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
    let body = pair.encode_request(lossy).unwrap().body;
    assert_eq!(
        body["generationConfig"]["responseMimeType"],
        "application/json"
    );
    assert_eq!(
        body["generationConfig"]["responseJsonSchema"]["properties"]["answer"]["type"],
        "string"
    );
    assert_eq!(
        body["generationConfig"]["responseJsonSchema"]["additionalProperties"],
        false
    );
    assert_eq!(body["contents"][0]["parts"][0]["text"], "hello");
}

#[test]
fn open_responses_client_omits_assistant_image_output() {
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
            source: stravia_runtime_contract::protocol::ir::MediaSource::Url(
                "https://example.test/image.png".into(),
            ),
            detail: None,
            cache_control: None,
        }]),
        tool_calls: None,
        tool_call_id: None,
        meta: None,
    });

    response
        .items
        .push(AiItem::output_text("image description"));
    let encoded = pair.encode_response(&response).unwrap();
    assert_eq!(
        encoded["output"][0]["content"][0]["text"],
        "image description"
    );
    assert!(!encoded.to_string().contains("image.png"));
}

#[test]
fn open_responses_client_omits_unsupported_tool_output_blocks() {
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
            source: stravia_runtime_contract::protocol::ir::MediaSource::Base64 {
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
            source: stravia_runtime_contract::protocol::ir::MediaSource::Url(
                "https://example.test/image.png".into(),
            ),
            detail: Some("future".into()),
            cache_control: None,
        }]),
        tool_calls: None,
        tool_call_id: Some("call_2".into()),
        meta: None,
    });

    response.items.push(AiItem::output_text("preserved"));
    let encoded = pair.encode_response(&response).unwrap();
    assert!(encoded.to_string().contains("preserved"));
    assert!(!encoded.to_string().contains("image.png"));
}

#[test]
fn google_empty_thought_does_not_shift_streamed_text_and_late_signature() {
    let mut decoder = ProtocolTransform::global()
        .decode_stream(GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA)
        .unwrap();
    let mut accumulator = crate::accumulator::StreamResponseAccumulator::default();
    for chunk in [
        json!({"candidates": [{"content": {"parts": [{"thought": true, "text": ""}]}}]}),
        json!({"candidates": [{"content": {"parts": [{"text": "STREAM_OK"}]}}]}),
        json!({"candidates": [{"content": {"parts": [{"text": "", "thoughtSignature": "real-signature"}]}, "finishReason": "STOP"}]}),
    ] {
        for delta in decoder
            .decode_chunk(format!("data: {chunk}\n\n").as_bytes())
            .unwrap()
        {
            accumulator.apply_with_identity(&delta);
        }
    }
    for delta in decoder.finish().unwrap() {
        accumulator.apply_with_identity(&delta);
    }
    let (response, ordinals) = accumulator.into_ai_response_with_ordinals();
    assert_eq!(ordinals, [0, 1]);
    assert_eq!(response.items[0].output_text_ref(), Some("STREAM_OK"));
    assert_eq!(
        response.items[1].thinking_ref(),
        Some(("", Some("real-signature")))
    );
}

#[test]
fn aggregated_google_stream_keeps_metadata_out_of_four_protocol_content() {
    let mut decoder = ProtocolTransform::global()
        .decode_stream(GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA)
        .unwrap();
    let mut accumulator = crate::accumulator::StreamResponseAccumulator::default();
    for chunk in [
        json!({
            "candidates": [{"content": {"role": "model", "parts": [{"text": "REA"}]}}],
            "modelVersion": "gemini-3.5-flash-lite",
            "responseId": "google-live-response",
            "usageMetadata": {"promptTokenCount": 7, "totalTokenCount": 7}
        }),
        json!({
            "candidates": [{"content": {"role": "model", "parts": [{"text": "DY"}]}, "finishReason": "STOP"}],
            "modelVersion": "gemini-3.5-flash-lite",
            "responseId": "google-live-response",
            "usageMetadata": {"promptTokenCount": 7, "candidatesTokenCount": 2, "totalTokenCount": 9}
        }),
    ] {
        let frame = format!("data: {chunk}\n\n");
        accumulator.apply_all(&decoder.decode_chunk(frame.as_bytes()).unwrap());
    }
    accumulator.apply_all(&decoder.finish().unwrap());
    let response = accumulator.into_ai_response();
    assert_eq!(response.items[0].output_text_ref(), Some("READY"));
    assert!(
        response
            .items
            .iter()
            .all(|item| item.unknown_ref().is_none())
    );
    assert_eq!(response.usage.prompt_tokens, 7);
    assert_eq!(response.usage.completion_tokens, 2);

    for ingress in [
        OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
        OPEN_RESPONSES_2026_04_24,
        ANTHROPIC_MESSAGES_2023_06_01,
        GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA,
    ] {
        let pair = ProtocolTransform::global()
            .bind(ingress, GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA)
            .unwrap();
        let encoded = pair.encode_response(&response).unwrap();
        let text = match ingress {
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1 => &encoded["choices"][0]["message"]["content"],
            OPEN_RESPONSES_2026_04_24 => &encoded["output"][0]["content"][0]["text"],
            ANTHROPIC_MESSAGES_2023_06_01 => &encoded["content"][0]["text"],
            GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA => {
                assert_eq!(encoded["responseId"], "google-live-response");
                assert_eq!(encoded["modelVersion"], "gemini-3.5-flash-lite");
                assert_eq!(encoded["usageMetadata"]["totalTokenCount"], 9);
                &encoded["candidates"][0]["content"]["parts"][0]["text"]
            }
            _ => unreachable!(),
        };
        assert_eq!(text, "READY");
    }
}

#[test]
fn canonical_google_stream_omits_unknown_items_and_preserves_text() {
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

    let (_, mut encoder) = pair.stream().unwrap().into_parts();
    let events = encoder
        .encode_deltas(&[
            metadata,
            unknown_item,
            AiStreamDelta::TextDelta("preserved".into()),
        ])
        .unwrap();
    assert!(events.iter().any(|event| event.data.contains("preserved")));
    assert!(
        events
            .iter()
            .all(|event| !event.data.contains("executable_code"))
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

    let response = AiResponse {
        items: vec![AiItem::thinking("", Some("opaque".into()))],
        ..AiResponse::new("response", "model")
    };
    assert!(pair.encode_response(&response).is_ok());
    let (_, mut encoder) = pair.stream().unwrap().into_parts();
    assert!(
        encoder
            .encode_deltas(&[AiStreamDelta::ThinkingSignature("opaque".into())])
            .is_ok()
    );
}

#[test]
fn gemini_stream_encode_omits_invalid_tool_arguments() {
    let pair = ProtocolTransform::global()
        .bind(
            GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA,
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
        )
        .expect("registered protocol pair");
    let (_decoder, mut encoder) = pair.stream().expect("stream session").into_parts();

    encoder
        .encode_deltas(&[
            AiStreamDelta::ToolCallStart {
                index: 0,
                id: "call_passthrough".into(),
                name: "local_probe".into(),
            },
            AiStreamDelta::ToolCallDelta {
                index: 0,
                arguments: "{\"path\":\"/tmp/x\"".into(),
            },
        ])
        .expect("mid-stream fragments stay pending until done");
    let events = encoder
        .encode_deltas(&[
            AiStreamDelta::TextDelta("preserved text".into()),
            AiStreamDelta::Done {
                stop_reason: "stop".into(),
            },
        ])
        .unwrap();
    assert!(
        events
            .iter()
            .any(|event| event.data.contains("preserved text"))
    );
    assert!(
        events
            .iter()
            .all(|event| !event.data.contains("functionCall"))
    );
    assert!(events.iter().any(|event| event.data.contains("STOP")));
}

#[test]
fn gemini_stream_encode_omits_argumentless_tool_call() {
    let pair = ProtocolTransform::global()
        .bind(
            GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA,
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
        )
        .expect("registered protocol pair");
    let (_decoder, mut encoder) = pair.stream().expect("stream session").into_parts();

    encoder
        .encode_deltas(&[AiStreamDelta::ToolCallStart {
            index: 0,
            id: "call_probe".into(),
            name: "local_probe".into(),
        }])
        .expect("tool call start stays pending");
    let events = encoder
        .encode_deltas(&[
            AiStreamDelta::TextDelta("preserved text".into()),
            AiStreamDelta::Done {
                stop_reason: "stop".into(),
            },
        ])
        .unwrap();
    assert!(
        events
            .iter()
            .any(|event| event.data.contains("preserved text"))
    );
    assert!(
        events
            .iter()
            .all(|event| !event.data.contains("functionCall"))
    );
    assert!(events.iter().any(|event| event.data.contains("STOP")));
}

#[test]
fn request_boundary_preserves_mixed_content_and_drops_invalid_calls_with_their_results() {
    for target in [
        ANTHROPIC_MESSAGES_2023_06_01,
        GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA,
    ] {
        let pair = ProtocolTransform::global()
            .bind(OPEN_RESPONSES_2026_04_24, target)
            .unwrap();
        for arguments in ["", "{", "[]", "null", "42"] {
            let mut mixed = AiItem::output_text("preserved assistant");
            mixed.content = MessageContent::Blocks(vec![
                ContentBlock::Text {
                    text: "preserved assistant".to_owned().into(),
                    cache_control: None,
                },
                ContentBlock::Video {
                    source: MediaSource::Url("https://example.test/video.mp4".into()),
                    media_type: Some("video/mp4".into()),
                },
            ]);
            mixed.tool_calls = Some(vec![
                ToolCall {
                    id: "bad-call".into(),
                    name: "invalid_probe".into(),
                    arguments: arguments.into(),
                },
                ToolCall {
                    id: "good-call".into(),
                    name: "valid_probe".into(),
                    arguments: "{\"key\":\"value\"}".into(),
                },
            ]);
            let mut bad_result = AiItem::output_text("unsupported associated result");
            bad_result.role = Role::Tool;
            bad_result.tool_call_id = Some("bad-call".into());
            let mut good_result = AiItem::output_text("preserved associated result");
            good_result.role = Role::Tool;
            good_result.tool_call_id = Some("good-call".into());
            let request = AiRequest::new(
                "model",
                vec![
                    mixed,
                    bad_result,
                    good_result,
                    AiItem {
                        role: Role::User,
                        content: MessageContent::Text("continue".to_owned().into()),
                        tool_calls: None,
                        tool_call_id: None,
                        meta: None,
                    },
                ],
            );
            let body = pair.encode_request(request).unwrap().body;
            let wire = body.to_string();
            assert!(wire.contains("preserved assistant"));
            assert!(wire.contains("valid_probe"));
            assert!(wire.contains("preserved associated result"));
            assert!(wire.contains("continue"));
            assert!(!wire.contains("invalid_probe"));
            assert!(!wire.contains("bad-call"));
            assert!(!wire.contains("unsupported associated result"));
            if target == ANTHROPIC_MESSAGES_2023_06_01 {
                assert!(!wire.contains("video.mp4"));
            } else {
                assert!(wire.contains("video.mp4"));
            }
        }
    }
}

#[test]
fn response_and_stream_boundaries_select_first_candidate() {
    let pair = ProtocolTransform::global()
        .bind(
            ANTHROPIC_MESSAGES_2023_06_01,
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
        )
        .unwrap();
    let response = pair.decode_response(json!({
        "id":"multi", "model":"model",
        "choices":[
            {"index":0,"message":{"role":"assistant","content":"first candidate"},"finish_reason":"stop"},
            {"index":1,"message":{"role":"assistant","content":"second candidate"},"finish_reason":"stop"}
        ]
    })).unwrap();
    let encoded = pair.encode_response(&response).unwrap();
    assert!(encoded.to_string().contains("first candidate"));
    assert!(!encoded.to_string().contains("second candidate"));

    let (mut decoder, mut encoder) = pair.stream().unwrap().into_parts();
    let chunk = json!({"id":"multi", "model":"model", "choices":[
        {"index":0,"delta":{"role":"assistant","content":"first candidate"},"finish_reason":null},
        {"index":1,"delta":{"role":"assistant","content":"second candidate"},"finish_reason":null}
    ]});
    let deltas = decoder
        .decode_chunk(format!("data: {chunk}\n\n").as_bytes())
        .unwrap();
    let events = encoder.encode_deltas(&deltas).unwrap();
    assert!(
        events
            .iter()
            .any(|event| event.data.contains("first candidate"))
    );
    assert!(
        events
            .iter()
            .all(|event| !event.data.contains("second candidate"))
    );
}

#[test]
fn invalid_source_wire_still_errors() {
    let pair = ProtocolTransform::global()
        .bind(
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            ANTHROPIC_MESSAGES_2023_06_01,
        )
        .unwrap();
    assert!(matches!(
        pair.decode_request(json!({"model":"model", "messages":"not an array"})),
        Err(TransformError::Wire { .. })
    ));
}

#[test]
fn chat_to_anthropic_stream_projects_only_complete_object_tool_inputs() {
    let pair = ProtocolTransform::global()
        .bind(
            ANTHROPIC_MESSAGES_2023_06_01,
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
        )
        .unwrap();
    let (mut decoder, mut encoder) = pair.stream().unwrap().into_parts();
    let mut events = Vec::new();
    let frames = [
        json!({"id":"chat_1","model":"model","choices":[{"index":0,"delta":{
        "content":"<think>thinking</think>answer",
        "tool_calls":[
            {"index":7,"id":"call_object_a","function":{"name":"lookup","arguments":" {\"query\":"}},
            {"index":2,"id":"call_array","function":{"name":"array","arguments":"[1,"}},
            {"index":9,"id":"call_object_b","function":{"name":"other","arguments":"{\"ok\":"}},
            {"index":4,"id":"call_bad","function":{"name":"bad","arguments":"{oops"}},
            {"index":5,"id":"call_null","function":{"name":"null","arguments":"null"}}
        ]}}]}),
        json!({"choices":[{"index":0,"delta":{"tool_calls":[
            {"index":9,"function":{"arguments":"true}"}},
            {"index":2,"function":{"arguments":"2]"}},
            {"index":7,"function":{"arguments":"\"hello\"} "}}
        ]}}]}),
        json!({"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}),
        json!({"choices":[],"usage":{"prompt_tokens":11,"completion_tokens":17,"total_tokens":28}}),
    ];
    for frame in frames {
        let wire = format!("data: {frame}\n\n");
        for chunk in wire.as_bytes().chunks(7) {
            events.extend(
                encoder
                    .encode_deltas(&decoder.decode_chunk(chunk).unwrap())
                    .unwrap(),
            );
        }
        assert!(
            !events
                .iter()
                .any(|event| event.data.contains("\"tool_use\"")),
            "tools must wait for a complete stream terminal, not a finish_reason"
        );
    }
    assert!(events.iter().any(|event| event.data.contains("answer")));
    assert!(
        events
            .iter()
            .any(|event| event.data.contains("thinking_delta"))
    );
    events.extend(
        encoder
            .encode_deltas(&decoder.decode_chunk(b"data: [DONE]\n\n").unwrap())
            .unwrap(),
    );
    events.extend(encoder.encode_deltas(&decoder.finish().unwrap()).unwrap());
    events.extend(encoder.finish().unwrap());

    // Reconstruct inputs the way an Anthropic SSE consumer does: concatenate
    // partial_json by content block index and parse on content_block_stop.
    let mut pending = std::collections::BTreeMap::new();
    let mut tools = std::collections::BTreeMap::new();
    let mut terminal = None;
    for event in &events {
        let data: Value = serde_json::from_str(&event.data).unwrap();
        let index = data["index"].as_u64().unwrap_or(0);
        match data["type"].as_str().unwrap() {
            "content_block_start" if data["content_block"]["type"] == "tool_use" => {
                pending.insert(
                    index,
                    (
                        data["content_block"]["id"].as_str().unwrap().to_owned(),
                        String::new(),
                    ),
                );
            }
            "content_block_delta" if data["delta"]["type"] == "input_json_delta" => {
                pending
                    .get_mut(&index)
                    .unwrap()
                    .1
                    .push_str(data["delta"]["partial_json"].as_str().unwrap());
            }
            "content_block_stop" => {
                if let Some((id, raw)) = pending.remove(&index) {
                    let input: Value =
                        serde_json::from_str(&raw).expect("SDK-consumable object input");
                    assert!(input.is_object());
                    tools.insert(id, (input, raw));
                }
            }
            "message_delta" => terminal = Some(data),
            _ => {}
        }
    }
    assert!(pending.is_empty());
    assert_eq!(tools.len(), 2);
    assert_eq!(
        tools["call_object_a"],
        (json!({"query":"hello"}), " {\"query\":\"hello\"} ".into())
    );
    assert_eq!(tools["call_object_b"].0, json!({"ok":true}));
    for bad in ["call_array", "call_bad", "call_null", "{oops"] {
        assert!(!events.iter().any(|event| event.data.contains(bad)));
    }
    let terminal = terminal.unwrap();
    assert_eq!(terminal["usage"]["input_tokens"], 11);
    assert_eq!(terminal["usage"]["output_tokens"], 17);
    assert_eq!(terminal["delta"]["stop_reason"], "tool_use");
    assert!(
        events
            .iter()
            .any(|event| event.event.as_deref() == Some("message_stop"))
    );
}

#[test]
fn anthropic_projection_validates_explicit_tool_completions() {
    let pair = ProtocolTransform::global()
        .bind(
            ANTHROPIC_MESSAGES_2023_06_01,
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
        )
        .unwrap();
    let (_, mut encoder) = pair.stream().unwrap().into_parts();
    for (index, id, arguments) in [
        (3, "call_complete", "{\"value\":1}"),
        (4, "call_nonobject", "[]"),
        (5, "call_malformed", "{"),
    ] {
        assert!(
            encoder
                .encode_deltas(&[
                    AiStreamDelta::ToolCallStart {
                        index,
                        id: id.into(),
                        name: "lookup".into()
                    },
                    AiStreamDelta::ToolCallDelta {
                        index,
                        arguments: arguments.into()
                    },
                ])
                .unwrap()
                .is_empty()
        );
        let events = encoder
            .encode_deltas(&[AiStreamDelta::ToolCallComplete {
                index,
                tool_call: ToolCall {
                    id: id.into(),
                    name: "lookup".into(),
                    arguments: arguments.into(),
                },
            }])
            .unwrap();
        assert_eq!(
            events.iter().any(|event| event.data.contains(id)),
            index == 3
        );
    }
    let terminal = encoder
        .encode_deltas(&[AiStreamDelta::Done {
            stop_reason: "stop".into(),
        }])
        .unwrap();
    assert!(
        !terminal
            .iter()
            .any(|event| event.data.contains("call_complete")),
        "explicitly completed calls must not be replayed at global completion"
    );
}

#[test]
fn chat_to_anthropic_stream_does_not_complete_tools_on_eof_or_error() {
    let pair = ProtocolTransform::global()
        .bind(
            ANTHROPIC_MESSAGES_2023_06_01,
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
        )
        .unwrap();
    for error in [false, true] {
        let (mut decoder, mut encoder) = pair.stream().unwrap().into_parts();
        let frame = json!({"choices":[{"index":0,"delta":{"content":"answer","tool_calls":[
            {"index":0,"id":"call_unfinished","function":{"name":"lookup","arguments":"{\"ok\":true}"}}
        ]}}]});
        let mut events = encoder
            .encode_deltas(
                &decoder
                    .decode_chunk(format!("data: {frame}\n\n").as_bytes())
                    .unwrap(),
            )
            .unwrap();
        if error {
            events.extend(
                encoder
                    .encode_deltas(
                        &decoder
                            .decode_chunk(
                                b"data: {\"error\":{\"message\":\"upstream failed\"}}\n\n",
                            )
                            .unwrap(),
                    )
                    .unwrap(),
            );
        }
        events.extend(encoder.encode_deltas(&decoder.finish().unwrap()).unwrap());
        events.extend(encoder.finish().unwrap());
        assert!(events.iter().any(|event| event.data.contains("answer")));
        assert!(
            !events
                .iter()
                .any(|event| event.data.contains("call_unfinished"))
        );
        assert!(
            !events
                .iter()
                .any(|event| event.event.as_deref() == Some("message_stop"))
        );
        assert_eq!(
            events
                .iter()
                .any(|event| event.event.as_deref() == Some("error")),
            error
        );
    }
    let (mut decoder, _) = pair.stream().unwrap().into_parts();
    assert!(decoder.decode_chunk(b"data: not-json\n\n").is_err());
}

#[test]
fn native_anthropic_empty_tool_input_keeps_immediate_wire_delivery() {
    let pair = ProtocolTransform::global()
        .bind(ANTHROPIC_MESSAGES_2023_06_01, ANTHROPIC_MESSAGES_2023_06_01)
        .unwrap();
    let (mut decoder, mut encoder) = pair.stream().unwrap().into_parts();
    let wire = b"event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"call_native\",\"name\":\"lookup\",\"input\":{}}}\n\n";
    let mut events = Vec::new();
    for chunk in wire.chunks(5) {
        events.extend(
            encoder
                .encode_deltas(&decoder.decode_chunk(chunk).unwrap())
                .unwrap(),
        );
    }
    let start = events
        .iter()
        .find(|event| event.data.contains("call_native"))
        .unwrap();
    let data: Value = serde_json::from_str(&start.data).unwrap();
    assert_eq!(data["content_block"]["input"], json!({}));
    assert_eq!(data["content_block"]["id"], "call_native");
}

#[test]
fn gemini_stream_encode_passes_valid_tool_arguments() {
    let pair = ProtocolTransform::global()
        .bind(
            GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA,
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
        )
        .expect("registered protocol pair");
    let (_decoder, mut encoder) = pair.stream().expect("stream session").into_parts();

    let events = encoder
        .encode_deltas(&[
            AiStreamDelta::ToolCallStart {
                index: 0,
                id: "call_probe".into(),
                name: "local_probe".into(),
            },
            AiStreamDelta::ToolCallDelta {
                index: 0,
                arguments: "{\"path\":\"/tmp/x\"}".into(),
            },
            AiStreamDelta::Done {
                stop_reason: "stop".into(),
            },
        ])
        .expect("valid arguments still encode");
    assert!(
        events
            .iter()
            .any(|event| event.data.contains("functionCall"))
    );
    assert!(events.iter().any(|event| event.data.contains("STOP")));
}
