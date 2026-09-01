use super::*;

#[test]
fn all_generation_request_codecs_restore_thinking_marker_carriers() {
    let marker = "<!-- stravia-projection:hm_0123456789abcdefghij:text:0:start -->visible\
                  <!-- stravia-projection:hm_0123456789abcdefghij:text:0:end -->\
                  <!-- stravia-history-marker:hm_0123456789abcdefghij -->";
    let cases = [
        (
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            json!({
                "model": "test-model",
                "messages": [{
                    "role": "assistant",
                    "reasoning_content": marker,
                    "content": ""
                }]
            }),
        ),
        (
            OPEN_RESPONSES_2026_04_24,
            json!({
                "model": "test-model",
                "input": [{
                    "type": "reasoning",
                    "summary": [{"type": "summary_text", "text": marker}],
                    "content": []
                }]
            }),
        ),
        (
            ANTHROPIC_MESSAGES_2023_06_01,
            json!({
                "model": "test-model",
                "max_tokens": 16,
                "messages": [{
                    "role": "assistant",
                    "content": [{
                        "type": "thinking",
                        "thinking": marker
                    }]
                }]
            }),
        ),
        (
            GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA,
            json!({
                "contents": [{
                    "role": "model",
                    "parts": [{"text": marker, "thought": true}]
                }]
            }),
        ),
        (
            BEDROCK_CONVERSE_V1,
            json!({
                "modelId": "test-model",
                "messages": [{
                    "role": "assistant",
                    "content": [{
                        "reasoningContent": {
                            "reasoningText": {"text": marker}
                        }
                    }]
                }]
            }),
        ),
        (
            COHERE_CHAT_V2,
            json!({
                "model": "test-model",
                "messages": [{
                    "role": "assistant",
                    "content": [{"type": "thinking", "thinking": marker}]
                }]
            }),
        ),
        (
            WATSONX_TEXT_CHAT_V1,
            json!({
                "model_id": "test-model",
                "messages": [{
                    "role": "assistant",
                    "reasoning_content": marker,
                    "content": ""
                }]
            }),
        ),
        (
            GATEWAY_LANGUAGE_MODEL_V4,
            json!({
                "model": "test-model",
                "prompt": [{
                    "role": "assistant",
                    "content": [{"type": "reasoning", "text": marker}]
                }]
            }),
        ),
    ];

    for (protocol, body) in cases {
        let adapter = crate::protocol::registry::ProtocolRegistry::global()
            .adapter(&protocol)
            .expect("registered generation adapter");
        let request = adapter
            .decode_request(body)
            .unwrap_or_else(|error| panic!("{protocol} request decode failed: {error}"));

        assert_eq!(
            crate::history_marker::history_marker_references(&request.items),
            vec!["hm_0123456789abcdefghij".to_string()],
            "{protocol} must decode a native reasoning carrier as Thinking"
        );
        assert!(
            request.items.iter().all(|item| match &item.content {
                IrMessageContent::Text(text) => !text.contains("stravia-history-marker"),
                IrMessageContent::Blocks(blocks) => blocks.iter().all(|block| {
                    !matches!(block, IrContentBlock::Text { text, .. } if text.contains("stravia-history-marker"))
                }),
            }),
            "{protocol} must not decode the marker as ordinary Text"
        );
        let decoded = serde_json::to_string(&request.items).expect("serialize decoded items");
        assert!(
            decoded.contains(crate::history_marker::PROJECTION_DELIMITER_PREFIX)
                && decoded.contains("visible"),
            "{protocol} must preserve Projection Delimiters and visible bytes"
        );
    }
}
#[test]
fn minimax_reasoning_split_fallback_think_tag() {
    let mut ai_resp = IrAiResponse::new("resp_2", "minimax-m2.7");
    ai_resp.push_output_text("<think>plan first</think>run ls".to_string());
    ai_resp.stop_reason = Some("stop".to_string());

    normalize_response_reasoning(&mut ai_resp);
    assert_eq!(
        ai_resp.reasoning_items().next().map(|(text, _)| text),
        Some("plan first")
    );
    assert_eq!(ai_resp.output_text(), "run ls");
}
#[test]
fn non_reasoning_model_no_regression() {
    let mut ai_resp = IrAiResponse::new("resp_3", "plain-model");
    ai_resp.push_output_text("hello world".to_string());
    ai_resp.stop_reason = Some("stop".to_string());

    normalize_response_reasoning(&mut ai_resp);
    assert!(ai_resp.reasoning_items().next().is_none());
    assert_eq!(ai_resp.output_text(), "hello world");
}
