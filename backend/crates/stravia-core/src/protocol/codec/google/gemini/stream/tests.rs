use super::*;

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
        error: crate::protocol::ir::AiError::new(
            crate::protocol::ir::AiErrorKind::StreamMidError,
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
fn stream_formatter_preserves_reasoning_signature_and_tool_id() {
    let mut formatter = GoogleStreamFormatter::new();
    let events = formatter.format_deltas(&[
        AiStreamDelta::MessageStart {
            id: "response".into(),
            model: "model".into(),
        },
        AiStreamDelta::ReasoningSummaryDelta {
            text: "checked the repository".into(),
            obfuscation: None,
            output_index: Some(0),
            content_index: Some(0),
        },
        AiStreamDelta::ThinkingSignature("opaque-reasoning".into()),
        AiStreamDelta::ToolCallStart {
            index: 0,
            id: "call_read".into(),
            name: "read".into(),
        },
        AiStreamDelta::ToolCallDelta {
            index: 0,
            arguments: r#"{"path":"Cargo.toml"}"#.into(),
        },
    ]);

    let bodies = events
        .iter()
        .map(|event| serde_json::from_str::<Value>(&event.data).expect("event JSON"))
        .collect::<Vec<_>>();
    assert_eq!(
        bodies[0]["candidates"][0]["content"]["parts"][0],
        serde_json::json!({
            "text": "checked the repository",
            "thought": true
        })
    );
    assert_eq!(
        bodies[1]["candidates"][0]["content"]["parts"][0]["thoughtSignature"],
        "opaque-reasoning"
    );
    assert_eq!(
        bodies[2]["candidates"][0]["content"]["parts"][0]["functionCall"]["id"],
        "call_read"
    );
}

#[test]
fn stream_formatter_emits_completed_reasoning_signature() {
    let events = GoogleStreamFormatter::new().format_deltas(&[
        AiStreamDelta::MessageStart {
            id: "response".into(),
            model: "model".into(),
        },
        AiStreamDelta::ItemDone {
            index: 0,
            item: AiItem::reasoning(
                vec!["checked the repository".into()],
                Vec::new(),
                Some("opaque-reasoning".into()),
            ),
        },
    ]);

    let signatures = events
        .iter()
        .filter_map(|event| serde_json::from_str::<Value>(&event.data).ok())
        .filter_map(|body| {
            body["candidates"][0]["content"]["parts"][0]["thoughtSignature"]
                .as_str()
                .map(str::to_owned)
        })
        .collect::<Vec<_>>();

    assert_eq!(signatures, ["opaque-reasoning"]);
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
