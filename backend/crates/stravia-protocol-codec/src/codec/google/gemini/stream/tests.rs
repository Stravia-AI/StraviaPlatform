use super::*;

#[test]
fn gemini_tool_call_stop_is_anthropic_tool_use() {
    let pair = crate::transform::ProtocolTransform::global()
        .bind(
            stravia_runtime_contract::protocol::ids::ANTHROPIC_MESSAGES_2023_06_01,
            stravia_runtime_contract::protocol::ids::GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA,
        )
        .expect("Anthropic/Gemini protocol pair");
    let response = pair
        .decode_response(serde_json::json!({
            "candidates": [{
                "content": {"role": "model", "parts": [{
                    "functionCall": {"id": "call_add", "name": "add", "args": {"a": 17, "b": 25}}
                }]},
                "finishReason": "STOP"
            }]
        }))
        .expect("decode Gemini tool call");
    let output = pair
        .encode_response(&response)
        .expect("encode Anthropic tool call");
    assert_eq!(output["content"][0]["type"], "tool_use");
    assert_eq!(output["stop_reason"], "tool_use");
}

#[test]
fn gemini_tool_call_stop_stream_survives_later_text_and_terminal_chunk() {
    for (finish_reason, expected) in [("STOP", "tool_calls"), ("MAX_TOKENS", "length")] {
        let mut parser = GoogleStreamParser::new();
        parser
            .parse_chunk(&format!(
                "data: {}\n\n",
                serde_json::json!({"candidates": [{"content": {"role": "model", "parts": [{
                    "functionCall": {"id": "call_add", "name": "add", "args": {"a": 17, "b": 25}}
                }]}}]})
            ))
            .expect("parse tool call");
        parser
            .parse_chunk(&format!(
                "data: {}\n\n",
                serde_json::json!({"candidates": [{"content": {"role": "model", "parts": [{
                    "text": "Waiting for the tool."
                }]}}]})
            ))
            .expect("parse later text");
        let terminal = parser
            .parse_chunk(&format!(
                "data: {}\n\n",
                serde_json::json!({"candidates": [{"finishReason": finish_reason}]})
            ))
            .expect("parse terminal chunk");
        assert!(terminal.iter().any(|delta| matches!(
            delta,
            AiStreamDelta::Done { stop_reason } if stop_reason == expected
        )));
    }
}

#[test]
fn thinking_replay_gemini_tool_signature_output_round_trip() {
    let endpoint = stravia_runtime_contract::protocol::ids::GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA;
    let pair = crate::transform::ProtocolTransform::global()
        .bind(endpoint, endpoint)
        .expect("Gemini protocol pair");
    let call = serde_json::json!({
        "functionCall": {"id": "call_read", "name": "read", "args": {"path": "Cargo.toml"}},
        "thoughtSignature": "native-tool-signature"
    });
    let parsed = pair.decode_response(serde_json::json!({
        "candidates": [{"content": {"role": "model", "parts": [call.clone()]}, "finishReason": "STOP"}]
    })).expect("decode Gemini response");
    let formatted = pair
        .encode_response(&parsed)
        .expect("encode Gemini response");

    assert_eq!(
        formatted["candidates"][0]["content"]["parts"],
        serde_json::json!([call])
    );
}

#[test]
fn thinking_replay_gemini_tool_signature_stream_round_trip() {
    let call = serde_json::json!({
        "functionCall": {"id": "call_read", "name": "read", "args": {"path": "Cargo.toml"}},
        "thoughtSignature": "native-tool-signature"
    });
    let chunk = serde_json::json!({
        "candidates": [{"content": {"role": "model", "parts": [call.clone()]}, "finishReason": "STOP"}]
    });
    let deltas = GoogleStreamParser::new()
        .parse_chunk(&format!("data: {chunk}\n\n"))
        .expect("Gemini stream");
    let start = deltas
        .iter()
        .position(|delta| matches!(delta, AiStreamDelta::ToolCallStart { .. }))
        .expect("tool call start");
    let mut formatter = GoogleStreamFormatter::new();
    let mut events = formatter.format_deltas(&deltas[..start]);
    events.extend(formatter.format_deltas(&deltas[start..]));
    let terminal = events
        .iter()
        .map(|event| serde_json::from_str::<Value>(&event.data).expect("event JSON"))
        .find(|body| body["candidates"][0].get("finishReason").is_some())
        .expect("Gemini terminal event");
    assert_eq!(terminal["candidates"][0]["finishReason"], "STOP");
    let parts = events
        .iter()
        .map(|event| serde_json::from_str::<Value>(&event.data).expect("event JSON"))
        .flat_map(|body| {
            body["candidates"][0]["content"]["parts"]
                .as_array()
                .cloned()
                .unwrap_or_default()
        })
        .collect::<Vec<_>>();

    assert_eq!(parts, vec![call]);
}

#[test]
fn thinking_replay_gemini_tool_signature_after_unsigned_thought_same_chunk() {
    assert_tool_signature_after_unsigned_thought(false);
}

#[test]
fn thinking_replay_gemini_tool_signature_after_unsigned_thought_cross_chunk() {
    assert_tool_signature_after_unsigned_thought(true);
}

fn assert_tool_signature_after_unsigned_thought(cross_chunk: bool) {
    let thought = serde_json::json!({"text": "checked the repository", "thought": true});
    let call = serde_json::json!({
        "functionCall": {"id": "call_read", "name": "read", "args": {"path": "Cargo.toml"}},
        "thoughtSignature": "native-tool-signature"
    });
    let batches = if cross_chunk {
        vec![vec![thought.clone()], vec![call.clone()]]
    } else {
        vec![vec![thought.clone(), call.clone()]]
    };
    let mut parser = GoogleStreamParser::new();
    let mut formatter = GoogleStreamFormatter::new();
    let mut events = Vec::new();
    for parts in batches {
        let chunk = serde_json::json!({
            "candidates": [{"content": {"role": "model", "parts": parts}}]
        });
        let deltas = parser
            .parse_chunk(&format!("data: {chunk}\n\n"))
            .expect("Gemini stream");
        events.extend(formatter.format_deltas(&deltas));
    }
    let parts = events
        .iter()
        .map(|event| serde_json::from_str::<Value>(&event.data).expect("event JSON"))
        .flat_map(|body| {
            body["candidates"][0]["content"]["parts"]
                .as_array()
                .cloned()
                .unwrap_or_default()
        })
        .collect::<Vec<_>>();

    assert_eq!(parts, vec![thought, call]);
}

#[test]
fn thinking_replay_gemini_accumulated_unsigned_thought_and_signed_call() {
    assert_accumulated_native_gemini_replay(false);
}

#[test]
fn thinking_replay_gemini_accumulated_signed_thought_and_unsigned_call() {
    assert_accumulated_native_gemini_replay(true);
}

fn assert_accumulated_native_gemini_replay(thought_is_signed: bool) {
    let mut thought = serde_json::json!({"text": "checked the repository", "thought": true});
    let mut call = serde_json::json!({
        "functionCall": {"id": "call_read", "name": "read", "args": {"path": "Cargo.toml"}}
    });
    if thought_is_signed {
        thought["thoughtSignature"] = serde_json::json!("native-thought-signature");
    } else {
        call["thoughtSignature"] = serde_json::json!("native-call-signature");
    }
    let chunk = serde_json::json!({
        "candidates": [{"content": {"role": "model", "parts": [thought.clone(), call.clone()]}, "finishReason": "STOP"}]
    });
    let deltas = GoogleStreamParser::new()
        .parse_chunk(&format!("data: {chunk}\n\n"))
        .expect("native Gemini stream");
    let mut accumulator = crate::accumulator::StreamResponseAccumulator::default();
    accumulator.apply_all(&deltas);
    let response = accumulator.into_ai_response();
    let thinking = response
        .items
        .iter()
        .filter_map(|item| item.thinking_ref())
        .collect::<Vec<_>>();
    if thought_is_signed {
        assert_eq!(
            thinking,
            vec![("checked the repository", Some("native-thought-signature"))]
        );
    } else {
        assert_eq!(
            thinking,
            vec![
                ("checked the repository", None),
                ("", Some("native-call-signature"))
            ]
        );
        let call_index = response
            .items
            .iter()
            .position(|item| item.function_call_ref().is_some())
            .expect("accumulated function call");
        assert!(call_index > 0);
        assert_eq!(
            response.items[call_index - 1].thinking_ref(),
            Some(("", Some("native-call-signature")))
        );
    }
    let endpoint = stravia_runtime_contract::protocol::ids::GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA;
    let pair = crate::transform::ProtocolTransform::global()
        .bind(endpoint, endpoint)
        .expect("Gemini protocol pair");
    let formatted = pair
        .encode_response(&response)
        .expect("format accumulated response");
    assert_eq!(
        formatted["candidates"][0]["content"]["parts"],
        serde_json::json!([thought.clone(), call.clone()])
    );
    let request = pair
        .decode_request(serde_json::json!({
            "contents": [formatted["candidates"][0]["content"].clone()]
        }))
        .expect("decode native Gemini replay request");
    let replay = pair
        .encode_request(&request)
        .expect("encode native Gemini replay request");
    assert_eq!(
        replay.body["contents"][0]["parts"],
        serde_json::json!([thought, call])
    );
}

#[test]
fn response_formatter_reasoning_emits_unsigned_paragraphs() {
    let mut response = AiResponse::new("response", "model");
    response.items.push(AiItem::reasoning(
        vec!["first summary".into(), "second summary".into()],
        vec!["first content".into(), "second content".into()],
        Some("encrypted-content-not-a-gemini-signature".into()),
    ));
    let formatted = GoogleResponseFormatter.format_response(&response);
    assert_eq!(
        formatted["candidates"][0]["content"]["parts"],
        serde_json::json!([
            {"text": "first summary", "thought": true},
            {"text": "second summary", "thought": true},
            {"text": "first content", "thought": true},
            {"text": "second content", "thought": true}
        ])
    );
}

#[test]
fn usage_maps_cached_content_tokens_both_ways() {
    let mut parsed = extract_gemini_usage(&serde_json::json!({
        "usageMetadata": {
            "promptTokenCount": 100,
            "candidatesTokenCount": 20,
            "totalTokenCount": 120,
            "cachedContentTokenCount": 80
        }
    }));
    assert_eq!(parsed.cache_read_tokens, Some(80));
    parsed.cache_creation_tokens = Some(10);

    let formatted = google_usage_from_counts(&parsed);
    assert_eq!(formatted["promptTokenCount"], 100);
    assert_eq!(formatted["cachedContentTokenCount"], 80);
    assert!(formatted.get("cacheCreationTokenCount").is_none());
}

#[test]
fn usage_separates_candidate_and_reasoning_tokens() {
    let formatted = google_usage_from_counts(&Usage {
        prompt_tokens: 100,
        completion_tokens: 20,
        reasoning_tokens: Some(5),
        ..Usage::default()
    });

    assert_eq!(formatted["candidatesTokenCount"], 15);
    assert_eq!(formatted["thoughtsTokenCount"], 5);
    assert_eq!(formatted["totalTokenCount"], 120);
}

#[test]
fn stream_formatter_preserves_reasoning_and_cache_usage() {
    let mut formatter = GoogleStreamFormatter::new();
    let events = formatter.format_deltas(&[
        AiStreamDelta::Usage(Usage {
            prompt_tokens: 100,
            completion_tokens: 20,
            total_tokens: 120,
            required_components_known: true,
            cache_read_tokens: Some(80),
            reasoning_tokens: Some(5),
            ..Usage::default()
        }),
        AiStreamDelta::Done {
            stop_reason: "stop".into(),
        },
    ]);
    let body: Value =
        serde_json::from_str(&events.last().expect("terminal event").data).expect("event JSON");

    assert_eq!(body["usageMetadata"]["candidatesTokenCount"], 15);
    assert_eq!(body["usageMetadata"]["thoughtsTokenCount"], 5);
    assert_eq!(body["usageMetadata"]["cachedContentTokenCount"], 80);
    assert_eq!(body["usageMetadata"]["totalTokenCount"], 120);
}

#[test]
fn stream_formatter_encodes_canonical_stream_error() {
    let mut formatter = GoogleStreamFormatter::new();
    let events = formatter.format_deltas(&[AiStreamDelta::StreamError {
        error: stravia_runtime_contract::protocol::ir::AiError::new(
            stravia_runtime_contract::protocol::ir::AiErrorKind::StreamMidError,
            "stream aborted",
        )
        .with_status(500),
    }]);

    assert_eq!(events.len(), 1);
    assert_eq!(events[0].event, None);
    let body: Value = serde_json::from_str(&events[0].data).expect("error JSON");
    assert_eq!(body["error"]["code"], 500);
    assert_eq!(body["error"]["status"], "stream_mid_error");
    assert_eq!(body["error"]["message"], "stream aborted");
}

#[test]
fn response_formatter_preserves_native_thinking_signature() {
    let upstream = serde_json::json!({
        "candidates": [{"content": {"role": "model", "parts": [{
            "text": "checked the repository",
            "thought": true,
            "thoughtSignature": "native-thinking-signature"
        }]}, "finishReason": "STOP"}]
    });
    let parsed = GoogleResponseParser
        .parse_response(upstream)
        .expect("Gemini response");
    let formatted = GoogleResponseFormatter.format_response(&parsed);
    assert_eq!(
        formatted["candidates"][0]["content"]["parts"],
        serde_json::json!([{
            "text": "checked the repository",
            "thought": true,
            "thoughtSignature": "native-thinking-signature"
        }])
    );
}

#[test]
fn stream_formatter_does_not_bind_signature_across_text_boundary() {
    let mut formatter = GoogleStreamFormatter::new();
    let mut events = formatter.format_deltas(&[AiStreamDelta::ThinkingSignature(
        "unpaired-signature".into(),
    )]);
    events.extend(formatter.format_deltas(&[
        AiStreamDelta::TextDelta("visible answer".into()),
        AiStreamDelta::ToolCallStart {
            index: 0,
            id: "call_read".into(),
            name: "read".into(),
        },
        AiStreamDelta::ToolCallDelta {
            index: 0,
            arguments: r#"{"path":"Cargo.toml"}"#.into(),
        },
    ]));
    let parts = events
        .iter()
        .map(|event| serde_json::from_str::<Value>(&event.data).expect("event JSON"))
        .flat_map(|body| {
            body["candidates"][0]["content"]["parts"]
                .as_array()
                .cloned()
                .unwrap_or_default()
        })
        .collect::<Vec<_>>();
    let call = parts
        .iter()
        .find(|part| part.get("functionCall").is_some())
        .expect("tool call part");
    assert_eq!(
        call,
        &serde_json::json!({
            "functionCall": {"id": "call_read", "name": "read", "args": {"path": "Cargo.toml"}}
        })
    );
    assert!(
        parts
            .iter()
            .any(|part| part == &serde_json::json!({"text": "visible answer"}))
    );
}

#[test]
fn stream_formatter_completed_reasoning_emits_unsigned_paragraphs() {
    let events = GoogleStreamFormatter::new().format_deltas(&[
        AiStreamDelta::MessageStart {
            id: "response".into(),
            model: "model".into(),
        },
        AiStreamDelta::ItemDone {
            index: 0,
            item: AiItem::reasoning(
                vec![
                    "checked the repository".into(),
                    "compared alternatives".into(),
                ],
                vec!["selected the safe path".into()],
                Some("opaque-reasoning".into()),
            ),
        },
    ]);

    let parts = events
        .iter()
        .map(|event| serde_json::from_str::<Value>(&event.data).expect("event JSON"))
        .flat_map(|body| {
            body["candidates"][0]["content"]["parts"]
                .as_array()
                .cloned()
                .unwrap_or_default()
        })
        .collect::<Vec<_>>();

    assert_eq!(
        parts,
        vec![
            serde_json::json!({"text": "checked the repository", "thought": true}),
            serde_json::json!({"text": "compared alternatives", "thought": true}),
            serde_json::json!({"text": "selected the safe path", "thought": true}),
        ]
    );
}

#[test]
fn stream_parser_preserves_reasoning_signature_and_tool_id() {
    let chunk = serde_json::json!({
        "candidates": [{
            "content": {
                "role": "model",
                "parts": [
                    {
                        "text": "checked the repository",
                        "thought": true,
                        "thoughtSignature": "opaque-reasoning"
                    },
                    {
                        "functionCall": {
                            "id": "call_read",
                            "name": "read",
                            "args": {"path": "Cargo.toml"}
                        }
                    }
                ]
            },
            "finishReason": "STOP"
        }]
    });
    let mut parser = GoogleStreamParser::new();
    let deltas = parser
        .parse_chunk(&format!("data: {chunk}\n\n"))
        .expect("Gemini stream");

    assert!(deltas.iter().any(
            |delta| matches!(delta, AiStreamDelta::ThinkingDelta(text) if text == "checked the repository")
        ));
    assert!(deltas.iter().any(
            |delta| matches!(delta, AiStreamDelta::ThinkingSignature(signature) if signature == "opaque-reasoning")
        ));
    assert!(deltas.iter().any(|delta| matches!(
        delta,
        AiStreamDelta::ToolCallStart { id, name, .. }
            if id == "call_read" && name == "read"
    )));
}

#[test]
fn parse_and_format_response_preserves_inline_data_part() {
    let upstream = serde_json::json!({
        "candidates": [{
            "content": {
                "role": "model",
                "parts": [{
                    "inlineData": {
                        "mimeType": "image/png",
                        "data": "iVBORw0KGgoAAAANSUhEUgA"
                    }
                }]
            },
            "finishReason": "STOP"
        }],
        "usageMetadata": {
            "promptTokenCount": 24,
            "candidatesTokenCount": 1120,
            "totalTokenCount": 1144
        },
        "modelVersion": "gemini-3.1-flash-image-preview"
    });

    let parsed = GoogleResponseParser.parse_response(upstream).unwrap();
    let formatted = GoogleResponseFormatter.format_response(&parsed);

    assert_eq!(
        formatted["candidates"][0]["content"]["parts"][0]["inlineData"]["mimeType"],
        "image/png"
    );
    assert_eq!(
        formatted["candidates"][0]["content"]["parts"][0]["inlineData"]["data"],
        "iVBORw0KGgoAAAANSUhEUgA"
    );
    assert_eq!(formatted["usageMetadata"]["promptTokenCount"], 24);
    assert_eq!(formatted["usageMetadata"]["candidatesTokenCount"], 1120);
}

#[test]
fn parse_and_format_response_preserves_future_parts_and_metadata() {
    let upstream = serde_json::json!({
        "candidates": [{
            "content": {
                "role": "model",
                "futureContentField": {"keep": true},
                "parts": [
                    {"text": "hello", "futureTextField": 7},
                    {"futurePart": {"foo": "bar"}}
                ]
            },
            "finishReason": "STOP",
            "futureCandidateField": {"rank": 1}
        }],
        "usageMetadata": {
            "promptTokenCount": 24,
            "candidatesTokenCount": 1120,
            "totalTokenCount": 1144,
            "trafficType": "ON_DEMAND",
            "promptTokensDetails": [{"modality": "TEXT", "tokenCount": 24}],
            "candidatesTokensDetails": [{"modality": "IMAGE", "tokenCount": 1120}]
        },
        "modelVersion": "gemini-3.1-flash-image-preview",
        "responseId": "resp-future",
        "futureTopLevelField": {"trace": "abc"}
    });

    let parsed = GoogleResponseParser.parse_response(upstream).unwrap();
    let formatted = GoogleResponseFormatter.format_response(&parsed);

    assert_eq!(
        formatted["candidates"][0]["content"]["parts"][0]["futureTextField"],
        7
    );
    assert_eq!(
        formatted["candidates"][0]["content"]["parts"][1]["futurePart"]["foo"],
        "bar"
    );
    assert_eq!(
        formatted["candidates"][0]["content"]["futureContentField"]["keep"],
        true
    );
    assert_eq!(
        formatted["candidates"][0]["futureCandidateField"]["rank"],
        1
    );
    assert_eq!(formatted["futureTopLevelField"]["trace"], "abc");
    assert_eq!(formatted["usageMetadata"]["trafficType"], "ON_DEMAND");
    assert_eq!(
        formatted["usageMetadata"]["candidatesTokensDetails"][0]["modality"],
        "IMAGE"
    );
}

#[test]
fn stream_parser_and_formatter_preserve_inline_data_part() {
    let raw = concat!(
        "data: {",
        "\"candidates\":[{",
        "\"content\":{\"role\":\"model\",\"parts\":[{",
        "\"inlineData\":{\"mimeType\":\"image/png\",\"data\":\"iVBORw0KGgoAAAANSUhEUgAABY+yvQxDX\"}",
        "}]},",
        "\"finishReason\":\"STOP\"}],",
        "\"usageMetadata\":{\"promptTokenCount\":24,\"candidatesTokenCount\":1120,\"totalTokenCount\":1144},",
        "\"modelVersion\":\"gemini-3.1-flash-image-preview\"",
        "}\n\n"
    );

    let mut parser = GoogleStreamParser::new();
    let deltas = parser.parse_chunk(raw).unwrap();
    let mut formatter = GoogleStreamFormatter::new();
    let events = formatter.format_deltas(&deltas);

    let image_event = events
        .iter()
        .map(|event| serde_json::from_str::<Value>(&event.data).unwrap())
        .find(|value| {
            value["candidates"][0]["content"]["parts"]
                .as_array()
                .is_some_and(|parts| parts.iter().any(|part| part.get("inlineData").is_some()))
        })
        .expect("expected an SSE event containing the inlineData part");

    assert_eq!(
        image_event["candidates"][0]["content"]["parts"][0]["inlineData"]["mimeType"],
        "image/png"
    );
    assert_eq!(
        image_event["candidates"][0]["content"]["parts"][0]["inlineData"]["data"],
        "iVBORw0KGgoAAAANSUhEUgAABY+yvQxDX"
    );

    let usage_event = events
        .iter()
        .map(|event| serde_json::from_str::<Value>(&event.data).unwrap())
        .find(|value| value.get("usageMetadata").is_some())
        .expect("expected terminal usage event");
    assert_eq!(usage_event["usageMetadata"]["promptTokenCount"], 24);
    assert_eq!(usage_event["usageMetadata"]["candidatesTokenCount"], 1120);
}

#[test]
fn stream_parser_and_formatter_preserve_future_part_and_usage_details() {
    let chunk = serde_json::json!({
        "candidates": [{
            "content": {
                "role": "model",
                "parts": [{"futurePart": {"foo": "bar"}}]
            },
            "finishReason": "STOP"
        }],
        "usageMetadata": {
            "promptTokenCount": 24,
            "candidatesTokenCount": 1120,
            "totalTokenCount": 1144,
            "trafficType": "ON_DEMAND",
            "candidatesTokensDetails": [{"modality": "IMAGE", "tokenCount": 1120}]
        },
        "modelVersion": "gemini-3.1-flash-image-preview",
        "responseId": "stream-future"
    });
    let raw = format!("data: {chunk}\n\n");

    let mut parser = GoogleStreamParser::new();
    let deltas = parser.parse_chunk(&raw).unwrap();
    let mut formatter = GoogleStreamFormatter::new();
    let events = formatter.format_deltas(&deltas);

    let future_part_event = events
        .iter()
        .map(|event| serde_json::from_str::<Value>(&event.data).unwrap())
        .find(|value| {
            value["candidates"][0]["content"]["parts"]
                .as_array()
                .is_some_and(|parts| parts.iter().any(|part| part.get("futurePart").is_some()))
        })
        .expect("expected an SSE event containing the future part");
    assert_eq!(
        future_part_event["candidates"][0]["content"]["parts"][0]["futurePart"]["foo"],
        "bar"
    );

    let usage_event = events
        .iter()
        .map(|event| serde_json::from_str::<Value>(&event.data).unwrap())
        .find(|value| value.get("usageMetadata").is_some())
        .expect("expected terminal usage event");
    assert_eq!(usage_event["usageMetadata"]["trafficType"], "ON_DEMAND");
    assert_eq!(
        usage_event["usageMetadata"]["candidatesTokensDetails"][0]["modality"],
        "IMAGE"
    );
    assert_eq!(usage_event["responseId"], "stream-future");
}

#[test]
fn stream_parser_extracts_usage_from_non_sse_generate_content_response() {
    let raw = serde_json::json!({
        "candidates": [{
            "content": {
                "parts": [{
                    "text": "{\n \"\n}",
                    "thoughtSignature": "EtpxCtdxAQtnKrzuYidcoegpuXXkuA=="
                }],
                "role": "model"
            },
            "finishReason": "STOP",
            "index": 0
        }],
        "modelVersion": "gemini-3.5-flash",
        "responseId": "Q90OarbFKsXM-sAPuOH-8AE",
        "usageMetadata": {
            "candidatesTokenCount": 1408,
            "promptTokenCount": 10996,
            "promptTokensDetails": [{
                "modality": "TEXT",
                "tokenCount": 10996
            }],
            "serviceTier": "standard",
            "thoughtsTokenCount": 4649,
            "totalTokenCount": 17053
        }
    })
    .to_string();

    let mut parser = GoogleStreamParser::new();
    let initial = parser.parse_chunk(&raw).unwrap();
    assert!(
        initial.is_empty(),
        "bare JSON should be completed by finish"
    );

    let deltas = parser.finish().unwrap();
    let usage = deltas
        .iter()
        .find_map(|delta| match delta {
            AiStreamDelta::Usage(usage) => Some(usage),
            _ => None,
        })
        .expect("non-SSE streamGenerateContent response should emit usage");

    assert_eq!(usage.prompt_tokens, 10996);
    assert_eq!(usage.completion_tokens, 6057);
    assert_eq!(usage.total_tokens, 17053);
    assert!(deltas.iter().any(|delta| matches!(
        delta,
        AiStreamDelta::Done { stop_reason } if stop_reason == "stop"
    )));
}

#[test]
fn stream_parser_rejects_malformed_known_part_fields() {
    let raw = concat!(
        "data:{",
        "\"candidates\":[{\"content\":{\"parts\":[{\"text\":42}]}}],",
        "\"modelVersion\":\"gemini-test\"",
        "}\n\n"
    );
    let mut parser = GoogleStreamParser::new();

    let error = parser
        .parse_chunk(raw)
        .expect_err("a malformed typed text part must fail closed");

    assert!(
        error
            .to_string()
            .contains("invalid typed Gemini stream part")
    );
}

#[test]
fn stream_aggregate_preserves_text_media_text_order() {
    let image = serde_json::json!({
        "inlineData": {"mimeType": "image/png", "data": "iVBORw0KGgoAAAANSUhEUgA"}
    });
    let chunk = serde_json::json!({
        "candidates": [{
            "content": {"role": "model", "parts": [
                {"text": "alpha text"},
                image.clone(),
                {"text": "omega text"}
            ]},
            "finishReason": "STOP"
        }]
    });
    let deltas = GoogleStreamParser::new()
        .parse_chunk(&format!("data: {chunk}\n\n"))
        .expect("native Gemini stream");
    let mut accumulator = crate::accumulator::StreamResponseAccumulator::default();
    accumulator.apply_all(&deltas);
    let response = accumulator.into_ai_response();

    use stravia_runtime_contract::protocol::ir::{ContentBlock, MessageContent};
    let carries_media = |item: &AiItem| match &item.content {
        MessageContent::Blocks(blocks) => blocks.iter().any(|block| {
            matches!(
                block,
                ContentBlock::Image { .. }
                    | ContentBlock::Audio { .. }
                    | ContentBlock::Video { .. }
                    | ContentBlock::File { .. }
            )
        }),
        _ => false,
    };
    assert_eq!(
        response.items.len(),
        3,
        "text/media/text must stay three distinct items"
    );
    assert_eq!(response.items[0].output_text_ref(), Some("alpha text"));
    assert!(
        carries_media(&response.items[1]),
        "middle item must carry the inline media block"
    );
    assert_eq!(response.items[2].output_text_ref(), Some("omega text"));

    let endpoint = stravia_runtime_contract::protocol::ids::GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA;
    let pair = crate::transform::ProtocolTransform::global()
        .bind(endpoint, endpoint)
        .expect("Gemini protocol pair");
    let formatted = pair
        .encode_response(&response)
        .expect("format accumulated response");
    assert_eq!(
        formatted["candidates"][0]["content"]["parts"],
        serde_json::json!([
            {"text": "alpha text"},
            image,
            {"text": "omega text"}
        ])
    );
}

#[test]
fn responses_terminal_output_preserves_text_call_text_order() {
    let chunk = serde_json::json!({
        "candidates": [{
            "content": {"role": "model", "parts": [
                {"text": "alpha text"},
                {"functionCall": {"id": "call_read", "name": "read", "args": {"path": "Cargo.toml"}}},
                {"text": "omega text"}
            ]},
            "finishReason": "STOP"
        }]
    });
    let deltas = GoogleStreamParser::new()
        .parse_chunk(&format!("data: {chunk}\n\n"))
        .expect("native Gemini stream");

    let mut formatter = crate::codec::open_responses::stream::ResponsesStreamFormatter::new();
    let events = formatter.format_deltas(&deltas);
    let completed = events
        .iter()
        .find(|event| event.event.as_deref() == Some("response.completed"))
        .expect("terminal response.completed event");
    let body: Value = serde_json::from_str(&completed.data).expect("completed JSON");
    let output = body["response"]["output"]
        .as_array()
        .expect("terminal output items");

    assert_eq!(
        output.len(),
        3,
        "terminal output must be exactly [message, function_call, message]"
    );
    for item in output {
        if item["type"] == "message" {
            assert!(
                item["content"]
                    .as_array()
                    .is_some_and(|content| !content.is_empty()),
                "terminal output must not contain empty message items"
            );
        }
    }
    assert_eq!(output[0]["type"], "message");
    let content_a = output[0]["content"].as_array().unwrap();
    assert_eq!(content_a.len(), 1);
    assert_eq!(content_a[0]["type"], "output_text");
    assert_eq!(content_a[0]["text"], "alpha text");
    assert_eq!(output[1]["type"], "function_call");
    assert_eq!(output[1]["call_id"], "call_read");
    assert_eq!(output[1]["name"], "read");
    assert_eq!(
        serde_json::from_str::<Value>(output[1]["arguments"].as_str().unwrap()).unwrap(),
        serde_json::json!({"path":"Cargo.toml"})
    );
    assert_eq!(output[2]["type"], "message");
    let content_b = output[2]["content"].as_array().unwrap();
    assert_eq!(content_b.len(), 1);
    assert_eq!(content_b[0]["type"], "output_text");
    assert_eq!(content_b[0]["text"], "omega text");
}

#[test]
fn responses_stream_encode_rejects_unrepresentable_media_item() {
    let chunk = serde_json::json!({
        "candidates": [{
            "content": {"role": "model", "parts": [
                {"text": "alpha text"},
                {"inlineData": {"mimeType": "image/png", "data": "iVBORw0KGgoAAAANSUhEUgA"}},
                {"text": "omega text"}
            ]},
            "finishReason": "STOP"
        }]
    });
    let deltas = GoogleStreamParser::new()
        .parse_chunk(&format!("data: {chunk}\n\n"))
        .expect("native Gemini stream");

    let pair = crate::transform::ProtocolTransform::global()
        .bind(
            stravia_runtime_contract::protocol::ids::OPEN_RESPONSES_2026_04_24,
            stravia_runtime_contract::protocol::ids::GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA,
        )
        .expect("Gemini to Responses protocol pair");
    let (_, mut encoder) = pair.stream().expect("stream session").into_parts();
    let error = encoder
        .encode_deltas(&deltas)
        .expect_err("Gemini inlineData has no Responses output carrier");
    assert!(matches!(
        error,
        crate::transform::TransformError::Unrepresentable { .. }
    ));
}
