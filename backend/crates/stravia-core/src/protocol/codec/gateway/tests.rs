use super::*;
use crate::protocol::ir::{AiRequest, MessageContent};

#[test]
fn encodes_ai_sdk_v4_language_model_wire() {
    let request = AiRequest::new(
        "ignored-by-gateway",
        vec![AiItem {
            role: Role::User,
            content: MessageContent::Text("hello".into()),
            tool_calls: None,
            tool_call_id: None,
            meta: None,
        }],
    );
    let (body, headers) = GatewayLanguageModelV4.encode_request(&request).unwrap();

    assert_eq!(
        GatewayLanguageModelV4.request_path("anything", false),
        "/language-model"
    );
    assert_eq!(
        body["prompt"][0],
        json!({"role": "user", "content": [{"type": "text", "text": "hello"}]})
    );
    assert!(body.get("model").is_none());
    assert!(body.get("messages").is_none());
    assert_eq!(
        headers
            .get("ai-language-model-specification-version")
            .unwrap(),
        "4"
    );
    assert_eq!(
        headers.get("ai-language-model-id").unwrap(),
        "ignored-by-gateway"
    );
    assert_eq!(headers.get("ai-language-model-streaming").unwrap(), "false");

    let mut streaming_request = request;
    streaming_request.stream.enabled = true;
    let (_, streaming_headers) = GatewayLanguageModelV4
        .encode_request(&streaming_request)
        .unwrap();
    assert_eq!(
        streaming_headers
            .get("ai-language-model-streaming")
            .unwrap(),
        "true"
    );
}

#[test]
fn decodes_ai_sdk_v4_response() {
    let response = GatewayLanguageModelV4
            .decode_response(json!({
                "content": [
                    {"type": "text", "text": "hello"},
                    {"type": "tool-call", "toolCallId": "call_1", "toolName": "weather", "input": {"city": "Paris"}}
                ],
                "finishReason": {"unified": "tool-calls"},
                "usage": {
                    "inputTokens": {"total": 3},
                    "outputTokens": {"total": 5}
                },
                "response": {"id": "resp_1", "modelId": "openai/gpt-5"}
            }))
            .unwrap();

    assert_eq!(response.id, "resp_1");
    assert_eq!(response.model, "openai/gpt-5");
    assert_eq!(response.output_text(), "hello");
    assert_eq!(response.tool_calls().count(), 1);
    assert_eq!(response.stop_reason.as_deref(), Some("tool-calls"));
    assert_eq!(response.usage.total_tokens, 8);
}

#[test]
fn parses_ai_sdk_v4_stream_events() {
    let mut parser = GatewayStreamParser::new();
    let deltas = parser
            .parse_chunk(
                "data: {\"type\":\"response-metadata\",\"id\":\"resp_1\",\"modelId\":\"openai/gpt-5\"}\n\n\
                 data: {\"type\":\"text-delta\",\"id\":\"text_1\",\"delta\":\"hello\"}\n\n\
                 data: {\"type\":\"finish\",\"finishReason\":{\"unified\":\"stop\"},\"usage\":{\"inputTokens\":{\"total\":3},\"outputTokens\":{\"total\":5}}}\n\n",
            )
            .unwrap();

    assert!(matches!(deltas[0], AiStreamDelta::MessageStart { .. }));
    assert!(matches!(&deltas[1], AiStreamDelta::TextDelta(text) if text == "hello"));
    assert!(matches!(
        deltas[2],
        AiStreamDelta::Usage(Usage {
            total_tokens: 8,
            ..
        })
    ));
    assert!(matches!(&deltas[3], AiStreamDelta::Done { stop_reason } if stop_reason == "stop"));
}
